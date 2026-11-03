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

use serde_json::{json, Value};

use crate::api::boundaries;
use crate::opencode::acp::{AllowOncePolicy, Approval, ApprovalPolicy, PermissionAsk};

// NOTE (M9): there is no longer a "path-decidable tool" list. A `path`
// predicate is decided from the ask's `locations`, whatever the tool — a
// `shell` ask puts its command there, an `edit` ask its file, and an
// `external_directory` ask the path pattern. Keeping a tool allowlist meant a
// rule was skipped for tools it named but did not list, which is how the vault
// boundary missed `Bash`.

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

/// The ruleset amux asks opencode to enforce for a NEW session (M9).
///
/// MEASURED 2026-11-03 on 2.2.4: without an `ask` rule opencode auto-approves,
/// so the policy never sees anything and boundary enforcement is inert. The
/// actions come from opencode's own permission vocabulary, not ours:
///
/// | boundary rules name | permission action to ask on |
/// |---|---|
/// | `Bash` | `shell` — `resources[0]` is the command |
/// | `Write`/`Edit`/… | `edit` — `metadata.files[].patch` is the diff |
/// | any rule with a `path` predicate | `external_directory` — the path pattern |
///
/// Derived from the ENABLED registry, so a box with every boundary disabled
/// gets an empty ruleset and pays nothing. Everything is `ask` rather than
/// `deny`: a boundary decision must be logged with its reason, and opencode's
/// `deny` would refuse before amux could say why.
pub fn session_permission_ruleset() -> Value {
    let (registry, _source, _errors) = boundaries::load();
    let (mut shell, mut edit, mut extdir) = (false, false, false);
    for boundary in registry.iter() {
        if boundary.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let Some(rules) = boundary.get("deny").and_then(|v| v.as_array()) else {
            continue;
        };
        for rule in rules.iter().filter_map(|r| r.as_table()) {
            if rule.contains_key("path") {
                extdir = true;
            }
            for tool in rule
                .get("tools")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
            {
                match tool.to_ascii_lowercase().as_str() {
                    "bash" | "shell" => shell = true,
                    "write" | "edit" | "multiedit" | "notebookedit" => edit = true,
                    "read" | "grep" | "glob" | "ls" | "list" => extdir = true,
                    _ => {}
                }
            }
        }
    }
    let mut out = Vec::new();
    for (action, needed) in [("shell", shell), ("edit", edit), ("external_directory", extdir)] {
        if needed {
            out.push(json!({"action": action, "resource": "*", "effect": "ask"}));
        }
    }
    Value::Array(out)
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
                // Predicates are evaluated in the order that matches what the
                // registry MEANS, and a refused call names the first one that
                // fires. Each predicate is decisive where it applies; only a
                // predicate whose INPUT is absent makes the rule undecidable.
                let path_rule = rule.get("path").and_then(|v| v.as_str());
                let content_rule = rule.get("content_any").and_then(|v| v.as_str());
                let needs_command =
                    rule.contains_key("match_all") || rule.contains_key("match_none");

                // (a) A path match is decisive ON ITS OWN. The registry's path
                // predicates are written for tools that carry paths, and an ask
                // that names the path has already answered the question — which
                // is why the vault rule refuses a write to `…/vault/x` without
                // needing a shell command to inspect.
                if let Some(p) = path_rule {
                    match compile(p) {
                        Some(re) => {
                            if ask.locations.iter().any(|loc| re.is_match(loc)) {
                                return Decision {
                                    approval: Approval::Reject,
                                    refused_by: Some((id.to_string(), reason.to_string())),
                                    matched_path: ask.locations.first().cloned(),
                                    undecidable: decision.undecidable,
                                };
                            }
                        }
                        None => {
                            warn_bad_regex(id, p);
                            decision.undecidable.push(id.to_string());
                            continue;
                        }
                    }
                }

                // (b) content_any — the text about to be written, from the
                // `edit` patch. Decidable only when the ask carries it.
                if let Some(p) = content_rule {
                    match ask.input.as_deref() {
                        Some(text) => {
                            if compile(p).is_some_and(|re| re.is_match(text)) {
                                return Decision {
                                    approval: Approval::Reject,
                                    refused_by: Some((id.to_string(), reason.to_string())),
                                    matched_path: None,
                                    undecidable: decision.undecidable,
                                };
                            }
                        }
                        None => {
                            decision.undecidable.push(id.to_string());
                            continue;
                        }
                    }
                }

                // (c) match_all / match_none — the shell command, which a
                // `shell` ask carries verbatim in `resources[0]`.
                if needs_command {
                    match ask.input.as_deref() {
                        None => {
                            decision.undecidable.push(id.to_string());
                            continue;
                        }
                        Some(text) => {
                            let all_ok = rule
                                .get("match_all")
                                .and_then(|v| v.as_array())
                                .map(|pats| {
                                    pats.iter().filter_map(|v| v.as_str()).all(|p| {
                                        compile(p).is_some_and(|re| re.is_match(text))
                                    })
                                })
                                .unwrap_or(true);
                            let none_hit = rule
                                .get("match_none")
                                .and_then(|v| v.as_array())
                                .map(|pats| {
                                    pats.iter()
                                        .filter_map(|v| v.as_str())
                                        .any(|p| compile(p).is_some_and(|re| re.is_match(text)))
                                })
                                .unwrap_or(false);
                            if all_ok && !none_hit {
                                return Decision {
                                    approval: Approval::Reject,
                                    refused_by: Some((id.to_string(), reason.to_string())),
                                    matched_path: None,
                                    undecidable: decision.undecidable,
                                };
                            }
                        }
                    }
                }

                // (d) `tools` alone: the rule refuses the tool outright.
                if path_rule.is_none() && content_rule.is_none() && !needs_command {
                    return Decision {
                        approval: Approval::Reject,
                        refused_by: Some((id.to_string(), reason.to_string())),
                        matched_path: None,
                        undecidable: decision.undecidable,
                    };
                }
            }
        }
        decision
    }
}

/// Compile a rule regex the way `boundaries.toml` documents it: Python `re`,
/// case-insensitive. `None` means the pattern is broken — which is reported by
/// [`warn_bad_regex`] and treated as unevaluable, never as a match.
fn compile(pattern: &str) -> Option<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

fn warn_bad_regex(boundary: &str, pattern: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let key = format!("{boundary}/{pattern}");
    if seen.lock().map(|mut s| s.insert(key)).unwrap_or(false) {
        tracing::warn!(
            boundary,
            pattern,
            verdict = "boundary_rule_unusable",
            "a boundaries.toml regex does not compile; rules using it are NOT enforced"
        );
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
            input: None,
            option_ids: vec!["once".into(), "always".into(), "reject".into()],
        }
    }

    /// An ask carrying the call's INPUT, in the shapes MEASURED on 2.2.4
    /// (2026-11-03): `shell` puts the command in `resources[0]`, `edit` puts the
    /// diff in `metadata.files[].patch`.
    fn ask_with_input(action: &str, resources: &[&str], input: &str) -> PermissionAsk {
        PermissionAsk {
            tool: action.to_string(),
            title: resources.first().map(|p| p.to_string()),
            kind: Some("tool".into()),
            locations: resources.iter().map(|p| p.to_string()).collect(),
            input: Some(input.to_string()),
            option_ids: vec!["once".into(), "always".into(), "reject".into()],
        }
    }

    /// The payments boundary, verbatim from `boundaries.toml`.
    fn payments_registry() -> Vec<toml::Table> {
        let text = r#"
[[boundary]]
id = "payments"
[[boundary.deny]]
tools = ["Bash"]
match_all = ['\b(curl|wget|http|httpie|python3?|node|ruby)\b', '(api\.stripe\.com|api\.brex\.com|api(-m)?\.paypal\.com)']
reason = "Payments go through `amux order` only."
"#;
        let t: toml::Table = toml::from_str(text).unwrap();
        t.get("boundary").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect()).unwrap()
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
    /// M9's headline: the payments rule — `match_all` on a bash command — is now
    /// DECIDABLE, because a `shell` ask carries the command in `resources[0]`.
    /// Before M9 this exact call was allowed with a `boundary_not_decidable`
    /// log line, which is why the gap mattered.
    #[test]
    fn a_shell_ask_is_refused_by_a_command_rule_now_that_the_command_is_carried() {
        let policy = BoundaryPolicy::from_registry(payments_registry());
        let d = policy.decide_detailed(&ask_with_input(
            "shell",
            &["curl -s https://api.stripe.com/v1/charges -u k:"],
            "curl -s https://api.stripe.com/v1/charges -u k:",
        ));
        assert_eq!(d.approval, Approval::Reject, "{d:?}");
        let (boundary, _reason) = d.refused_by.clone().expect("names its boundary");
        assert_eq!(boundary, "payments");
        assert!(d.undecidable.is_empty(), "{d:?}");

        // The registry's own ALLOW example: reading about stripe is not paying.
        let ok = policy.decide_detailed(&ask_with_input(
            "shell",
            &["grep -rn api.stripe.com docs/"],
            "grep -rn api.stripe.com docs/",
        ));
        assert_eq!(ok.approval, Approval::AllowOnce, "{ok:?}");
    }

    /// `content_any` becomes decidable from an `edit` ask's patch — the text
    /// about to be written, measured as a unified diff.
    #[test]
    fn an_edit_ask_is_refused_by_a_content_rule_when_the_patch_carries_it() {
        let text = r#"
[[boundary]]
id = "secrets"
[[boundary.deny]]
tools = ["Write", "Edit"]
content_any = 'sk_live_[A-Za-z0-9]+'
reason = "a live secret must never be written to a file"
"#;
        let parsed: toml::Table = toml::from_str(text).unwrap();
        let reg: Vec<toml::Table> = parsed.get("boundary").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect()).unwrap();
        let policy = BoundaryPolicy::from_registry(reg);
        let patch = "Index: k.txt\n@@ -0,0 +1,1 @@\n+sk_live_ABC123\n";
        let d = policy.decide_detailed(&ask_with_input("edit", &["k.txt"], patch));
        assert_eq!(d.approval, Approval::Reject, "the written secret must be refused: {d:?}");

        // Writing something harmless on the same rule is allowed.
        let ok = policy.decide_detailed(&ask_with_input("edit", &["k.txt"], "+hello\n"));
        assert_eq!(ok.approval, Approval::AllowOnce, "{ok:?}");
    }

    /// An ask with NO input still cannot decide a command rule — the fallback
    /// has to survive, or a transport that carries less would silently start
    /// allowing (or refusing) everything.
    #[test]
    fn without_input_a_command_rule_is_still_reported_as_undecidable() {
        let policy = BoundaryPolicy::from_registry(payments_registry());
        let d = policy.decide_detailed(&ask("shell", &[]));
        assert_eq!(d.approval, Approval::AllowOnce, "{d:?}");
        assert_eq!(d.undecidable, vec!["payments".to_string()], "{d:?}");
    }

    /// The ruleset is what makes any of this fire: without an `ask` rule
    /// opencode auto-approves and the policy is never consulted.
    #[test]
    fn the_session_ruleset_asks_for_the_actions_the_registry_polices() {
        let rs = session_permission_ruleset();
        let actions: Vec<&str> = rs
            .as_array()
            .expect("a ruleset array")
            .iter()
            .filter_map(|r| r.get("action").and_then(|v| v.as_str()))
            .collect();
        assert!(actions.contains(&"shell"), "Bash rules need shell asks: {actions:?}");
        assert!(actions.contains(&"edit"), "Write/Edit rules need edit asks: {actions:?}");
        assert!(
            actions.contains(&"external_directory"),
            "a path rule needs path asks: {actions:?}"
        );
        // Every rule is `ask`, never `deny`: a boundary decision must be logged
        // with its reason, and opencode's deny would refuse before amux can say
        // why.
        for rule in rs.as_array().unwrap() {
            assert_eq!(rule.get("effect").and_then(|v| v.as_str()), Some("ask"), "{rule}");
            assert_eq!(rule.get("resource").and_then(|v| v.as_str()), Some("*"), "{rule}");
        }
    }

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
