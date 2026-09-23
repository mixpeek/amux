//! Capability-policy vocabulary and pure decision engine.
//!
//! Every side effect is described with the same [`ActionContext`] and judged
//! by [`CapabilityPolicy::decide`].  Provider/API adapters may differ in how
//! they execute work, but they do not get to invent a second authorization
//! vocabulary.  The server persists the resulting [`CapabilityDecision`] as
//! a receipt before execution.

use serde::{Deserialize, Serialize};

/// Durable responsibility attached to an actor by the planning plane.
///
/// Roles are authorization inputs, not prompt labels.  In particular, a
/// planner cannot accidentally acquire coding authority just because a policy
/// file contains a broad allow rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    RootPlanner,
    Subplanner,
    Worker,
    Verifier,
}

impl AgentRole {
    pub fn is_planner(self) -> bool {
        matches!(self, Self::RootPlanner | Self::Subplanner)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Trusted,
    /// Imported/retrieved/user-controlled text.  The default is deliberately
    /// the weaker value so an old context fragment cannot deserialize as
    /// trusted merely because it predates the field.
    #[default]
    Untrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    Read,
    ExecuteTask,
    ToolUse,
    FileWrite,
    ExternalWrite,
    ConnectorWrite,
    GitPush,
    Deploy,
    Delete,
    CapabilityChange,
}

impl ActionClass {
    pub fn is_external(self) -> bool {
        matches!(
            self,
            ActionClass::ExternalWrite
                | ActionClass::ConnectorWrite
                | ActionClass::GitPush
                | ActionClass::Deploy
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ActionClass::Read => "read",
            ActionClass::ExecuteTask => "execute_task",
            ActionClass::ToolUse => "tool_use",
            ActionClass::FileWrite => "file_write",
            ActionClass::ExternalWrite => "external_write",
            ActionClass::ConnectorWrite => "connector_write",
            ActionClass::GitPush => "git_push",
            ActionClass::Deploy => "deploy",
            ActionClass::Delete => "delete",
            ActionClass::CapabilityChange => "capability_change",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityEffect {
    #[default]
    Allow,
    Ask,
    Deny,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRule {
    pub id: String,
    pub effect: CapabilityEffect,
    /// Empty means every action.
    #[serde(default)]
    pub actions: Vec<ActionClass>,
    /// Exact actor names. Empty means every actor.
    #[serde(default)]
    pub actors: Vec<String>,
    /// Planning roles. Empty means every role (including actors without one).
    #[serde(default)]
    pub roles: Vec<AgentRole>,
    /// Resource prefixes (paths, API routes, domains, account ids).
    #[serde(default)]
    pub resource_prefixes: Vec<String>,
    #[serde(default)]
    pub trust: Option<TrustLevel>,
    #[serde(default)]
    pub reversible: Option<bool>,
    /// The server enforces this over persisted receipts for the matching rule.
    #[serde(default)]
    pub max_per_window: Option<u32>,
    #[serde(default)]
    pub window_secs: Option<u64>,
    #[serde(default)]
    pub max_cost_microusd: Option<u64>,
    pub rationale: String,
}

impl CapabilityRule {
    pub fn matches(&self, ctx: &ActionContext) -> bool {
        (self.actions.is_empty() || self.actions.contains(&ctx.action))
            && (self.actors.is_empty() || self.actors.iter().any(|a| a == &ctx.actor))
            && (self.roles.is_empty() || ctx.role.is_some_and(|role| self.roles.contains(&role)))
            && (self.resource_prefixes.is_empty()
                || self
                    .resource_prefixes
                    .iter()
                    .any(|prefix| ctx.resource.starts_with(prefix)))
            && self.trust.is_none_or(|trust| trust == ctx.trust)
            && self
                .reversible
                .is_none_or(|reversible| reversible == ctx.reversible)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPolicy {
    pub version: String,
    #[serde(default)]
    pub default_effect: CapabilityEffect,
    #[serde(default)]
    pub rules: Vec<CapabilityRule>,
}

impl Default for CapabilityPolicy {
    fn default() -> Self {
        Self {
            version: "builtin-1".into(),
            // Compatibility default for installations without a policy file.
            // The untrusted/irreversible hard boundary below still applies.
            default_effect: CapabilityEffect::Allow,
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionContext {
    pub actor: String,
    #[serde(default)]
    pub role: Option<AgentRole>,
    pub action: ActionClass,
    pub resource: String,
    pub trust: TrustLevel,
    pub reversible: bool,
    #[serde(default)]
    pub cost_microusd: u64,
    /// True when content is attempting to add tools, permissions, policy
    /// rules, approval, or another authority-bearing fact.
    #[serde(default)]
    pub capability_expansion: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDecision {
    pub effect: CapabilityEffect,
    pub policy_version: String,
    pub rule_id: Option<String>,
    pub rationale: String,
    pub rate_limit: Option<RateLimitSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitSpec {
    pub max_per_window: u32,
    pub window_secs: u64,
}

impl CapabilityPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.version.trim().is_empty() {
            return Err("policy version is required".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for rule in &self.rules {
            if rule.id.trim().is_empty() || !ids.insert(rule.id.as_str()) {
                return Err(format!(
                    "policy rule ids must be non-empty and unique: {}",
                    rule.id
                ));
            }
            if rule.rationale.trim().is_empty() {
                return Err(format!("policy rule {} requires a rationale", rule.id));
            }
            match (rule.max_per_window, rule.window_secs) {
                (Some(0), _) | (_, Some(0)) => {
                    return Err(format!("policy rule {} has a zero rate limit", rule.id));
                }
                (Some(_), None) | (None, Some(_)) => {
                    return Err(format!(
                        "policy rule {} must set both max_per_window and window_secs",
                        rule.id
                    ));
                }
                _ => {}
            }
            if rule.max_cost_microusd == Some(0) {
                return Err(format!("policy rule {} has a zero cost limit", rule.id));
            }
        }
        Ok(())
    }

    pub fn decide(&self, ctx: &ActionContext) -> CapabilityDecision {
        // Planner/worker separation is a hard system boundary, not a prompt
        // reminder and not something a permissive installation policy may
        // weaken. Planners can read and mutate the planning control plane;
        // workers and verifiers retain the ordinary policy-driven behavior.
        let planner_control_plane = ctx.resource.starts_with("/api/harness/goals")
            || ctx.resource.starts_with("/api/harness/planning-nodes")
            || ctx.resource.starts_with("/api/harness/handoffs");
        if ctx.role.is_some_and(AgentRole::is_planner)
            && (!matches!(
                ctx.action,
                ActionClass::Read | ActionClass::CapabilityChange
            ) || (ctx.action == ActionClass::CapabilityChange && !planner_control_plane))
        {
            return CapabilityDecision {
                effect: CapabilityEffect::Deny,
                policy_version: self.version.clone(),
                rule_id: Some("builtin-planner-no-execution".into()),
                rationale: "planning roles may plan and delegate but cannot execute work or mutate delivery surfaces"
                    .into(),
                rate_limit: None,
            };
        }

        // Content never grants authority. This guard is deliberately before
        // configured rules: a rule may allow an action, but an untrusted
        // document cannot turn itself into a policy administrator.
        if ctx.trust == TrustLevel::Untrusted && ctx.capability_expansion {
            return CapabilityDecision {
                effect: CapabilityEffect::Deny,
                policy_version: self.version.clone(),
                rule_id: Some("builtin-untrusted-authority-boundary".into()),
                rationale: "untrusted content cannot expand capabilities or approve side effects"
                    .into(),
                rate_limit: None,
            };
        }

        let matching = self.rules.iter().find(|rule| rule.matches(ctx));

        // Retrieved text may propose an external/irreversible action, but a
        // trusted actor must explicitly approve the exact parameters. A deny
        // may tighten this boundary; an allow may not weaken it.
        if ctx.trust == TrustLevel::Untrusted && (ctx.action.is_external() || !ctx.reversible) {
            if let Some(rule) = matching.filter(|rule| rule.effect == CapabilityEffect::Deny) {
                return CapabilityDecision {
                    effect: CapabilityEffect::Deny,
                    policy_version: self.version.clone(),
                    rule_id: Some(rule.id.clone()),
                    rationale: rule.rationale.clone(),
                    rate_limit: None,
                };
            }
            return CapabilityDecision {
                effect: CapabilityEffect::Ask,
                policy_version: self.version.clone(),
                rule_id: matching
                    .map(|rule| rule.id.clone())
                    .or_else(|| Some("builtin-untrusted-side-effect".into())),
                rationale:
                    "untrusted input requires approval for an external or irreversible action"
                        .into(),
                rate_limit: None,
            };
        }

        if let Some(rule) = matching {
            if rule
                .max_cost_microusd
                .is_some_and(|limit| ctx.cost_microusd > limit)
            {
                return CapabilityDecision {
                    effect: CapabilityEffect::Deny,
                    policy_version: self.version.clone(),
                    rule_id: Some(rule.id.clone()),
                    rationale: format!(
                        "action cost {} microusd exceeds rule limit {}",
                        ctx.cost_microusd,
                        rule.max_cost_microusd.unwrap_or_default()
                    ),
                    rate_limit: None,
                };
            }
            return CapabilityDecision {
                effect: rule.effect,
                policy_version: self.version.clone(),
                rule_id: Some(rule.id.clone()),
                rationale: rule.rationale.clone(),
                rate_limit: match (rule.max_per_window, rule.window_secs) {
                    (Some(max_per_window), Some(window_secs)) => Some(RateLimitSpec {
                        max_per_window,
                        window_secs,
                    }),
                    _ => None,
                },
            };
        }

        CapabilityDecision {
            effect: self.default_effect,
            policy_version: self.version.clone(),
            rule_id: None,
            rationale: "policy default".into(),
            rate_limit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(action: ActionClass) -> ActionContext {
        ActionContext {
            actor: "worker-a".into(),
            role: None,
            action,
            resource: "/repo/main".into(),
            trust: TrustLevel::Trusted,
            reversible: true,
            cost_microusd: 0,
            capability_expansion: false,
        }
    }

    #[test]
    fn matching_rule_is_the_decision_and_carries_rate_contract() {
        let policy = CapabilityPolicy {
            version: "v7".into(),
            default_effect: CapabilityEffect::Deny,
            rules: vec![CapabilityRule {
                id: "local-writes".into(),
                effect: CapabilityEffect::Allow,
                actions: vec![ActionClass::FileWrite],
                actors: vec!["worker-a".into()],
                roles: vec![],
                resource_prefixes: vec!["/repo".into()],
                trust: Some(TrustLevel::Trusted),
                reversible: Some(true),
                max_per_window: Some(10),
                window_secs: Some(60),
                max_cost_microusd: None,
                rationale: "scoped local edit".into(),
            }],
        };
        let d = policy.decide(&ctx(ActionClass::FileWrite));
        assert_eq!(d.effect, CapabilityEffect::Allow);
        assert_eq!(d.rule_id.as_deref(), Some("local-writes"));
        assert_eq!(d.rate_limit.unwrap().max_per_window, 10);
    }

    #[test]
    fn untrusted_content_can_never_grant_itself_authority() {
        let policy = CapabilityPolicy {
            version: "permissive".into(),
            default_effect: CapabilityEffect::Allow,
            rules: vec![CapabilityRule {
                id: "allow-everything".into(),
                effect: CapabilityEffect::Allow,
                actions: vec![],
                actors: vec![],
                roles: vec![],
                resource_prefixes: vec![],
                trust: None,
                reversible: None,
                max_per_window: None,
                window_secs: None,
                max_cost_microusd: None,
                rationale: "test".into(),
            }],
        };
        let mut c = ctx(ActionClass::CapabilityChange);
        c.trust = TrustLevel::Untrusted;
        c.capability_expansion = true;
        assert_eq!(policy.decide(&c).effect, CapabilityEffect::Deny);
    }

    #[test]
    fn untrusted_external_action_requires_exact_approval_by_default() {
        let mut c = ctx(ActionClass::ExternalWrite);
        c.trust = TrustLevel::Untrusted;
        assert_eq!(
            CapabilityPolicy::default().decide(&c).effect,
            CapabilityEffect::Ask
        );
    }

    #[test]
    fn permissive_rule_cannot_bypass_untrusted_side_effect_boundary() {
        let mut c = ctx(ActionClass::GitPush);
        c.trust = TrustLevel::Untrusted;
        c.reversible = false;
        let policy = CapabilityPolicy {
            version: "permissive".into(),
            default_effect: CapabilityEffect::Allow,
            rules: vec![CapabilityRule {
                id: "allow-all".into(),
                effect: CapabilityEffect::Allow,
                actions: vec![],
                actors: vec![],
                roles: vec![],
                resource_prefixes: vec![],
                trust: None,
                reversible: None,
                max_per_window: None,
                window_secs: None,
                max_cost_microusd: None,
                rationale: "permissive test rule".into(),
            }],
        };
        assert_eq!(policy.decide(&c).effect, CapabilityEffect::Ask);
    }

    #[test]
    fn configured_cost_cap_denies_instead_of_falling_through_to_allow() {
        let policy = CapabilityPolicy {
            version: "costed".into(),
            default_effect: CapabilityEffect::Allow,
            rules: vec![CapabilityRule {
                id: "bounded-tool".into(),
                effect: CapabilityEffect::Allow,
                actions: vec![ActionClass::ToolUse],
                actors: vec![],
                roles: vec![],
                resource_prefixes: vec![],
                trust: None,
                reversible: None,
                max_per_window: None,
                window_secs: None,
                max_cost_microusd: Some(500),
                rationale: "bounded tool spend".into(),
            }],
        };
        let mut c = ctx(ActionClass::ToolUse);
        c.cost_microusd = 501;
        assert_eq!(policy.decide(&c).effect, CapabilityEffect::Deny);
    }

    #[test]
    fn invalid_partial_rate_contract_is_rejected() {
        let mut policy = CapabilityPolicy::default();
        policy.rules.push(CapabilityRule {
            id: "partial-rate".into(),
            effect: CapabilityEffect::Allow,
            actions: vec![],
            actors: vec![],
            roles: vec![],
            resource_prefixes: vec![],
            trust: None,
            reversible: None,
            max_per_window: Some(10),
            window_secs: None,
            max_cost_microusd: None,
            rationale: "invalid fixture".into(),
        });
        assert!(policy.validate().is_err());
    }

    #[test]
    fn planner_execution_is_denied_even_by_a_permissive_policy() {
        let mut c = ctx(ActionClass::ExecuteTask);
        c.role = Some(AgentRole::Subplanner);
        let decision = CapabilityPolicy::default().decide(&c);
        assert_eq!(decision.effect, CapabilityEffect::Deny);
        assert_eq!(
            decision.rule_id.as_deref(),
            Some("builtin-planner-no-execution")
        );
    }
}
