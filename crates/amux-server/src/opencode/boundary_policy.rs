//! Boundary-aware approvals: `boundaries.toml` answers opencode permission asks.
//!
//! ## The gap this closes
//!
//! `boundaries.toml` deny rules are enforced by `scripts/hooks/boundary-guard.py`,
//! a **PreToolUse hook** — Claude Code's mechanism. OpenCode has no hooks, so
//! before this module **no boundary was enforced for an opencode worker at
//! all**: the agent could write to the vault, or curl a payment provider, and
//! nothing refused it. The permission ask is the only interception point that
//! provider offers, and `acp::ApprovalPolicy`'s own doc comment says the seam
//! exists for exactly this.
//!
//! ## The hard constraint, measured before designing
//!
//! What an ask actually carries (`PermissionAsk`, decoded from both transports):
//! `tool`, `title`, `kind`, **`locations`** (paths), `option_ids`. There is **no
//! command text and no written content**, so:
//!
//! - rules with a **`path`** predicate are decidable for FILE tools
//!   (`Write`/`Edit`/`Read`/`Grep`/`Glob`/…), because the ask names the paths;
//! - rules that need the bash command (`match_all`/`match_none`) or the written
//!   text (`content_any`) are **not decidable here**. Denying every `Bash` call
//!   that touches such a rule would brick the worker; allowing it silently would
//!   be theatre. So they pass, and the pass is LOGGED ONCE per (boundary, rule)
//!   with `verdict = boundary_not_decidable` — a countable, sweepable gap rather
//!   than a silent one (two-fix rule).
//!
//! ## One registry, two evaluators
//!
//! This reads the same registry `api::boundaries` reads (installed file, else the
//! copy embedded in the binary, merged with the owner's `~/.amux/boundaries.toml`
//! by `id`). Two evaluators can drift; the mitigation is one registry plus a
//! parity corpus over the registry's own `block_examples`/`allow_examples` where
//! they are decidable — not a claim that they cannot.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::api::boundaries;
use crate::opencode::acp::{AllowOncePolicy, Approval, ApprovalPolicy, PermissionAsk};

/// File tools whose decisions can be made from the paths the ask carries.
///
/// Everything else (`Bash` above all) needs the command text, which no ask
/// provides — that is the whole reason this list exists rather than a blanket
/// "any deny rule that names the tool".
const PATH_DECIDABLE_TOOLS: &[&str] = &[
    "write",
    "edit",
    "multiedit",
    "notebookedit",
    "read",
    "grep",
    "glob",
    "ls",
    "list",
];

/// opencode's tool names → the names `boundaries.toml` writes rules against.
///
/// One pure function, because a mismatch here silently enforces NOTHING: the
/// rules say `Write`, the agent says `write`, and a case bug would look like a
/// clean allow.
pub fn boundary_tool_name(opencode_tool: &str) -> String {
    match opencode_tool.trim().to_ascii_lowercase().as_str() {
        "bash" | "shell" => "Bash".to_string(),
        "write" | "create" => "Write".to_string(),
        "edit" | "patch" | "str_replace" => "Edit".to_string(),
        "multiedit" => "MultiEdit".to_string(),
        "notebookedit" => "NotebookEdit".to_string(),
        "read" | "view" => "Read".to_string(),
        "grep" => "Grep".to_string(),
        "glob" | "find" => "Glob".to_string(),
        "ls" | "list" => "List".to_string(),
        other => {
            // Unknown tool: capitalise like the registry does. A rule written
            // for it then still matches; nothing silently becomes "Bash".
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => "Unknown".to_string(),
            }
        }
    }
}

/// What the policy decided, and why — the log line's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub approval: Approval,
    /// `Some((boundary, rule reason))` when a boundary refused the call.
    pub refused_by: Option<(String, String)>,
    /// The path that made the rule match — the one detail a reader needs to
    /// tell "refused correctly" from "refused the wrong file".
    pub matched_path: Option<String>,
    /// Rules that name this tool but whose predicate needs information an ask
    /// does not carry. Reported once each (see [`note_undecidable`]).
    pub undecidable: Vec<String>,
}

impl Decision {
    fn allow() -> Self {
        Self {
            approval: Approval::AllowOnce,
            refused_by: None,
            matched_path: None,
            undecidable: Vec::new(),
        }
    }
}

/// A boundary-aware [`ApprovalPolicy`].
///
/// Default consent is unchanged — `AllowOnce`, the same consent `--auto` gave —
/// so this can only ever REFUSE a call a boundary names; it never widens what
/// the agent may do. Only tools the worker's own opencode config marks `ask`
/// ever reach it at all.
pub struct BoundaryPolicy {
    registry: Vec<toml::Table>,
}

impl BoundaryPolicy {
    /// The registry as it currently resolves on this box.
    ///
    /// A registry that only PARTLY parses is announced: `api::boundaries` falls
    /// back to its embedded copy and reports the file that failed, and a policy
    /// that quietly enforced a stale copy would be worse than one that says so.
    pub fn load() -> Self {
        let (registry, source, errors) = boundaries::load();
        if !errors.is_empty() {
            tracing::warn!(
                errors = ?errors,
                boundaries = registry.len(),
                verdict = "boundaries.toml did not fully parse; a fallback registry is in force",
                "opencode boundaries registry degraded"
            );
        } else {
            tracing::info!(
                boundaries = registry.len(),
                source = source == boundaries::Source::Installed,
                "opencode boundaries loaded"
            );
        }
        Self { registry }
    }

    /// For tests: decide against an explicit registry, no filesystem.
    pub fn from_registry(registry: Vec<toml::Table>) -> Self {
        Self { registry }
    }

    /// Decide, and say why. Kept separate from [`ApprovalPolicy::decide`] so a
    /// caller can log the reason without re-deriving it.
    /// Note on `apply_to_isolated`: it is a HOOK-side concern — the guard knows
    /// a worker is isolated from `AMUX_ISOLATED_WORKER`. This policy has no
    /// worker identity, so it does not consult the flag and an isolated
    /// opencode worker still gets boundary refusals. That is the conservative
    /// direction and it costs nothing: per `CLAUDE.md`, a refusal injects no
    /// prompt and routes nothing.
    pub fn decide_detailed(&self, ask: &PermissionAsk) -> Decision {
        let tool = boundary_tool_name(&ask.tool);
        let path_decidable = PATH_DECIDABLE_TOOLS.contains(&ask.tool.trim().to_ascii_lowercase().as_str());
        let mut decision = Decision::allow();

        for boundary in self.registry.iter() {
            if boundary.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }
            let id = boundary.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let Some(rules) = boundary.get("deny").and_then(|v| v.as_array()) else {
                continue;
            };
            for rule in rules.iter().filter_map(|r| r.as_table()) {
                let names_tool = rule
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(|t| t.as_str())
                            .any(|t| t.eq_ignore_ascii_case(&tool))
                    })
                    .unwrap_or(false);
                if !names_tool {
                    continue;
                }
                let reason = rule.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                // A `path` predicate on a file tool: the ask names the paths, so
                // this is decidable. `content_any` makes it undecidable even
                // then — the rule only applies to certain content, which an ask
                // never carries.
                let has_content_rule = rule.get("content_any").is_some();
                let path = rule.get("path").and_then(|v| v.as_str());
                if let (Some(re), true) = (path, path_decidable) {
                    if has_content_rule {
                        decision.undecidable.push(id.to_string());
                        continue;
                    }
                    let Ok(re) = regex::RegexBuilder::new(re).case_insensitive(true).build() else {
                        // A rule we cannot compile must not silently pass: say
                        // so and keep it out of the "enforced" column.
                        tracing::warn!(
                            boundary = id,
                            pattern = re,
                            verdict = "boundary deny `path` regex does not compile; rule not enforced",
                            "boundary rule unusable"
                        );
                        continue;
                    };
                    if let Some(hit) = ask.locations.iter().find(|p| re.is_match(p)) {
                        return Decision {
                            approval: Approval::Reject,
                            refused_by: Some((id.to_string(), reason.to_string())),
                            matched_path: Some(hit.clone()),
                            undecidable: decision.undecidable,
                        };
                    }
                    continue;
                }
                if path.is_none() && !has_content_rule && rule.get("match_all").is_none() {
                    // `tools` alone: the rule refuses the tool outright.
                    return Decision {
                        approval: Approval::Reject,
                        refused_by: Some((id.to_string(), reason.to_string())),
                        matched_path: None,
                        undecidable: decision.undecidable,
                    };
                }
                // Needs the command text or the content: not in an ask.
                decision.undecidable.push(id.to_string());
            }
        }
        decision
    }
}

/// Log each (boundary, rule) undecidable gap ONCE per process.
///
/// Per-ask logging would bury the fleet in the same line, and silence would hide
/// a boundary that does not apply to this provider — which is what this module
/// exists to stop.
fn note_undecidable(boundary: &str, tool: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let key = format!("{boundary}/{tool}");
    let first = seen.lock().map(|mut s| s.insert(key)).unwrap_or(false);
    if first {
        tracing::warn!(
            boundary,
            tool,
            verdict = "boundary_not_decidable",
            reason = "an opencode permission ask carries tool and paths, not the command text or \
                      written content — this rule's match_all/match_none/content_any predicate \
                      cannot be evaluated here, so the call is allowed unchanged",
            "boundary rule not enforceable at ask time"
        );
    }
}

impl ApprovalPolicy for BoundaryPolicy {
    fn decide(&self, ask: &PermissionAsk) -> Approval {
        let decision = self.decide_detailed(ask);
        match &decision.refused_by {
            Some((boundary, reason)) => {
                tracing::warn!(
                    boundary,
                    tool = %ask.tool,
                    locations = ?ask.locations,
                    matched = decision.matched_path.as_deref().unwrap_or("-"),
                    reason,
                    verdict = "boundary_refused",
                    "boundary refused an opencode permission ask"
                );
            }
            None => {
                for boundary in &decision.undecidable {
                    note_undecidable(boundary, &ask.tool);
                }
            }
        }
        decision.approval
    }
}

/// The policy opencode protocols use: boundary-aware by default, with
/// `AMUX_OPENCODE_BOUNDARIES=0` as the documented escape hatch.
///
/// Default ON, because "no boundary applies to this provider" is the defect, not
/// the safe state. The escape hatch exists for a box debugging a false refusal,
/// and it announces itself.
pub fn active() -> Arc<dyn ApprovalPolicy> {
    // Built ONCE per process: a TOML re-parse on every permission ask is pure
    // waste, and the registry changing under a running worker is an owner edit
    // that a restart makes visible (and logs). The decision itself is cached,
    // not the ask: every ask is still evaluated.
    static ACTIVE: OnceLock<Arc<dyn ApprovalPolicy>> = OnceLock::new();
    ACTIVE
        .get_or_init(|| {
    let off = std::env::var("AMUX_OPENCODE_BOUNDARIES")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| matches!(v.as_str(), "0" | "false" | "off" | "no"))
        .is_some();
    if off {
        tracing::warn!(
            verdict = "AMUX_OPENCODE_BOUNDARIES is off; opencode permission asks are allowed \
                       without consulting boundaries.toml",
            "opencode boundaries disabled"
        );
        return Arc::new(AllowOncePolicy);
    }
    Arc::new(BoundaryPolicy::load())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(tool: &str, locations: &[&str]) -> PermissionAsk {
        PermissionAsk {
            tool: tool.to_string(),
            title: locations.first().map(|p| p.to_string()),
            kind: None,
            locations: locations.iter().map(|p| p.to_string()).collect(),
            option_ids: vec!["once".into(), "always".into(), "reject".into()],
        }
    }

    /// The registry's own vault rule, copied from `boundaries.toml` so the test
    /// cannot pass against a registry that changed shape underneath it.
    fn vault_registry() -> Vec<toml::Table> {
        let text = r#"
[[boundary]]
id = "credentials"
title = "Vault is owner-only"
rule = "never read the vault"
[[boundary.deny]]
tools = ["Bash", "Read", "Edit", "Write", "MultiEdit", "NotebookEdit", "Grep", "Glob"]
match_all = ['\.amux/(vault|ts-secrets)(/|\b)']
path = '\.amux/(vault|ts-secrets)/'
reason = "The vault holds card and secret data."
"#;
        let t: toml::Table = toml::from_str(text).unwrap();
        t.get("boundary")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect())
            .unwrap()
    }

    #[test]
    fn opencode_tool_names_map_to_the_registry_vocabulary() {
        assert_eq!(boundary_tool_name("bash"), "Bash");
        assert_eq!(boundary_tool_name("write"), "Write");
        assert_eq!(boundary_tool_name("edit"), "Edit");
        assert_eq!(boundary_tool_name("read"), "Read");
        assert_eq!(boundary_tool_name("  GREP "), "Grep");
        assert_eq!(boundary_tool_name("webfetch"), "Webfetch");
    }

    /// The decisive case: a WRITE into the vault is refused from the paths the
    /// ask carries — this is the whole point, because opencode has no hook.
    #[test]
    fn a_path_rule_refuses_a_file_tool_it_names() {
        let policy = BoundaryPolicy::from_registry(vault_registry());
        let d = policy.decide_detailed(&ask("write", &["/home/pv/.amux/vault/items.json"]));
        assert_eq!(d.approval, Approval::Reject, "{d:?}");
        let (boundary, reason) = d.refused_by.expect("a refusal names its boundary");
        assert_eq!(boundary, "credentials");
        assert!(reason.contains("vault"), "{reason}");
        assert_eq!(
            d.matched_path.as_deref(),
            Some("/home/pv/.amux/vault/items.json"),
            "the refusal must name the path that matched, for the log"
        );

        // The same tool on a path the rule does not name is untouched.
        let ok = policy.decide_detailed(&ask("write", &["/home/pv/dev/amux/docs/vault.md"]));
        assert_eq!(ok.approval, Approval::AllowOnce, "{ok:?}");
        assert_eq!(ok.refused_by, None);
    }

    /// A BASH call that the same rule names is NOT decided: the ask has no
    /// command text, so refusing would brick every shell call and allowing
    /// quietly would hide a boundary. It allows, and reports the gap.
    #[test]
    fn a_command_rule_is_reported_as_undecidable_not_silently_passed() {
        let policy = BoundaryPolicy::from_registry(vault_registry());
        let d = policy.decide_detailed(&ask("bash", &[]));
        assert_eq!(d.approval, Approval::AllowOnce, "consent is unchanged: {d:?}");
        assert_eq!(
            d.undecidable,
            vec!["credentials".to_string()],
            "the gap must be named, per boundary"
        );
    }

    /// `enabled = false` turns a boundary off, exactly as the registry documents.
    #[test]
    fn a_disabled_boundary_decides_nothing() {
        let mut reg = vault_registry();
        reg[0].insert("enabled".to_string(), toml::Value::Boolean(false));
        let policy = BoundaryPolicy::from_registry(reg);
        let d = policy.decide_detailed(&ask("write", &["/home/pv/.amux/vault/items.json"]));
        assert_eq!(d.approval, Approval::AllowOnce, "{d:?}");
        assert!(d.undecidable.is_empty(), "{d:?}");
    }

    /// A rule with no predicate beyond `tools` refuses that tool outright.
    #[test]
    fn a_tools_only_rule_refuses_the_tool() {
        let text = r#"
[[boundary]]
id = "no-shell"
[[boundary.deny]]
tools = ["Bash"]
reason = "shell is not allowed here"
"#;
        let t: toml::Table = toml::from_str(text).unwrap();
        let reg: Vec<toml::Table> = t
            .get("boundary")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect())
            .unwrap();
        let d = BoundaryPolicy::from_registry(reg).decide_detailed(&ask("bash", &[]));
        assert_eq!(d.approval, Approval::Reject, "{d:?}");
        assert_eq!(d.refused_by.unwrap().0, "no-shell");
    }

    /// The parity corpus: the registry's own `block_examples`/`allow_examples`
    /// that are PATH-decidable, run against this evaluator. The Python hook
    /// checks the whole corpus; this checks the part an ask can decide, so the
    /// two evaluators cannot drift on the paths without a test failing.
    #[test]
    fn the_registry_path_examples_agree_with_this_evaluator() {
        let (registry, _src, errors) = boundaries::load();
        assert!(errors.is_empty(), "registry must parse: {errors:?}");
        let mut checked = 0;
        for boundary in registry.iter() {
            let id = boundary.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let Some(rules) = boundary.get("deny").and_then(|v| v.as_array()) else {
                continue;
            };
            for rule in rules.iter().filter_map(|r| r.as_table()) {
                let Some(path_re) = rule.get("path").and_then(|v| v.as_str()) else {
                    continue;
                };
                // Examples are written for the Bash/file mix; only the ones
                // that ARE a bare path can be replayed as an ask's location.
                for ex in rule
                    .get("block_examples")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                {
                    let Some(p) = ex.split_whitespace().last() else {
                        continue;
                    };
                    if !p.starts_with('/') || p.contains(char::is_whitespace) {
                        continue;
                    }
                    // Only examples the rule's own path regex claims.
                    let Ok(re) = regex::RegexBuilder::new(path_re)
                        .case_insensitive(true)
                        .build()
                    else {
                        continue;
                    };
                    if !re.is_match(p) {
                        continue;
                    }
                    let policy = BoundaryPolicy::from_registry(registry.clone());
                    let d = policy.decide_detailed(&ask("write", &[p]));
                    assert_eq!(
                        d.approval,
                        Approval::Reject,
                        "boundary {id}: its own block_example {p} must be refused by this evaluator"
                    );
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 1,
            "the parity corpus must actually contain checkable examples, not zero"
        );
    }
}
