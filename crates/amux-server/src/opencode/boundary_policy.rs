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
    /// does not carry — the command, the written text, or a gate receipt.
    /// Reported once each (see [`note_undecidable`]).
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
                // ONE meaning for the registry: evaluate by the ask's KIND,
                // exactly as `scripts/hooks/boundary-guard.py::rule_applies`
                // does. Getting this wrong is not a near miss — it refuses the
                // registry's own allow_examples (review, 2026-11-05: an `ls` of
                // the token directory and a `cat` of an approval file, five in
                // all), because a `shell` ask carries its COMMAND in
                // `locations`, and a path regex matches a command that merely
                // names a guarded path.
                //
                //   shell  → match_all (all must match) and match_none (none
                //            may); `path` is NOT consulted.
                //   file   → `path` must match a location, then `content_any`
                //            when the rule has one; match_all/match_none are not
                //            consulted.
                //   other  → no rule applies (the hook returns False too).
                //
                // `external_directory` is the one kind the hook never sees (it
                // is opencode-specific): a read outside the workspace carries
                // the path pattern, so it is decided by `path` like a file tool.
                // Without it the vault is unreachable from the serve transport.
                let is_external = ask.tool.eq_ignore_ascii_case("external_directory");
                let file_kind = matches!(
                    tool.as_str(),
                    "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Read" | "Grep" | "Glob"
                        | "List" | "Ls"
                );
                let shell_kind = tool == "Bash";

                if !shell_kind && !file_kind && !is_external {
                    continue; // not a kind this registry describes
                }

                let matches = |pattern: &str, text: &str| -> Option<bool> {
                    // `fancy_regex::is_match` is `Result` (a backtrack limit is
                    // a runtime error, not a match): an unevaluable answer is
                    // `None`, which no caller treats as a match.
                    compile(pattern).and_then(|re| re.is_match(text).ok())
                };

                if shell_kind {
                    // A Bash rule with no `match_all` never applies (the hook
                    // requires one), and `path`/`content_any` are ignored.
                    let Some(patterns) = rule.get("match_all").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    let Some(command) = ask.input.as_deref() else {
                        // The command is what decides this rule; without it the
                        // answer is "unknown", never "allow".
                        decision.undecidable.push(id.to_string());
                        continue;
                    };
                    let mut all_ok = true;
                    for pat in patterns.iter().filter_map(|v| v.as_str()) {
                        match matches(pat, command) {
                            Some(true) => {}
                            Some(false) => {
                                all_ok = false;
                                break;
                            }
                            None => {
                                warn_bad_regex(id, pat);
                                all_ok = false;
                                break;
                            }
                        }
                    }
                    if !all_ok {
                        continue;
                    }
                    if rule.contains_key("requires_receipt") {
                        // A receipt-gated rule (tenant-workload-gate) refuses
                        // only the command segments whose target has no FRESH
                        // receipt — state the guard writes and validates after
                        // a read-only gate run, segment by segment. An ask
                        // carries the command and nothing else, so this
                        // evaluator cannot decide it, and pretending would be
                        // wrong in one direction or the other: refusing every
                        // matching command would refuse the registry's own
                        // allow examples (`kubectl -n kube-system rollout
                        // restart`, `kubectl -n monitoring scale`, a commit
                        // message quoting a scale), because match_all matches
                        // them too. Reported once per (boundary, tool), never
                        // silently passed. Follow-up: hand these rules to the
                        // guard, which is the only thing that can see receipts.
                        decision.undecidable.push(id.to_string());
                        continue;
                    }
                    let excluded = rule
                        .get("match_none")
                        .and_then(|v| v.as_array())
                        .map(|pats| {
                            pats.iter().filter_map(|v| v.as_str()).any(|p| {
                                matches(p, command).unwrap_or(false)
                            })
                        })
                        .unwrap_or(false);
                    if excluded {
                        continue;
                    }
                    return Decision {
                        approval: Approval::Reject,
                        refused_by: Some((id.to_string(), reason.to_string())),
                        matched_path: None,
                        undecidable: decision.undecidable,
                    };
                }

                // File and external-path asks: `path` is required, then
                // `content_any` where the rule has one.
                let Some(path_pattern) = rule.get("path").and_then(|v| v.as_str()) else {
                    continue; // a path rule is the only kind that applies here
                };
                // The guard reads a Grep/Glob argument as a DIRECTORY: it tests
                // the path with a trailing slash as well as the joined glob, so
                // `Grep ~/.amux/vault` hits a `\.amux/(vault|ts-secrets)/` rule
                // that the bare path would miss (`boundary-guard.py`, the
                // Grep/Glob branch). Same here, or the corpus disagrees on that
                // exact example.
                let directory_kind = matches!(tool.as_str(), "Grep" | "Glob");
                let hit = ask.locations.iter().find(|loc| {
                    matches(path_pattern, loc).unwrap_or(false)
                        || (directory_kind
                            && matches(path_pattern, &format!("{}/", loc.trim_end_matches('/')))
                                .unwrap_or(false))
                });
                let Some(hit) = hit else {
                    continue; // the path does not match: the rule does not apply
                };
                // `content_any` is a LIST of regexes in the registry (any one
                // matching applies the rule); a bare string is accepted as one
                // pattern. Reading only the string form is how the review's
                // `Edit ~/.claude/settings.json :: {"permissions": {}}` allow
                // example got refused: the rule stated `content_any` as a list,
                // the check was skipped, and the path alone decided.
                let content_patterns: Vec<&str> = match rule.get("content_any") {
                    Some(toml::Value::String(s)) => vec![s.as_str()],
                    Some(toml::Value::Array(items)) => {
                        items.iter().filter_map(|v| v.as_str()).collect()
                    }
                    _ => Vec::new(),
                };
                if !content_patterns.is_empty() {
                    let Some(text) = ask.input.as_deref() else {
                        decision.undecidable.push(id.to_string());
                        continue;
                    };
                    let mut applied = false;
                    for pat in &content_patterns {
                        match matches(pat, text) {
                            Some(true) => {
                                applied = true;
                                break;
                            }
                            Some(false) => {}
                            None => warn_bad_regex(id, pat),
                        }
                    }
                    if !applied {
                        continue;
                    }
                }
                return Decision {
                    approval: Approval::Reject,
                    refused_by: Some((id.to_string(), reason.to_string())),
                    matched_path: Some(hit.clone()),
                    undecidable: decision.undecidable,
                };
            }
        }
        decision
    }
}

/// Compile a rule regex the way `boundaries.toml` documents it: Python `re`,
/// case-insensitive.
///
/// **`fancy-regex`, not `regex`** — the registry is written for Python's `re`,
/// and three of its rules use lookbehind or lookahead
/// (`email`'s `(?<![\w.*-])`, `fleet-tmux`'s `(?!…)` and
/// `tenant-workload-gate`'s `(?=\s|$)`). `regex` cannot compile those, so those
/// rules were silently INERT here — including the one that refuses
/// `tmux kill-server`. `fancy-regex` is already in the dependency graph
/// (`syntect`'s `regex-fancy`), so this costs no extra build.
///
/// `None` means the pattern cannot be evaluated — reported by [`warn_bad_regex`]
/// and treated as unevaluable, never as a match.
fn compile(pattern: &str) -> Option<fancy_regex::Regex> {
    fancy_regex::RegexBuilder::new(pattern)
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

    /// The registry THIS COMMIT ships, not the one installed on the machine.
    ///
    /// `boundaries::load()` prefers `~/.amux/hooks/boundaries.toml` and merges
    /// `~/.amux/boundaries.toml` over it. That is right for ENFORCEMENT and
    /// wrong for a parity corpus: a machine whose install predates a boundary
    /// (a Mac that has no `tenant-workload-gate`) then fails these tests while
    /// CI — which has no installed file — passes, and the bytes under review are
    /// neither. `tests/env_config.rs` states the same rule for the integration
    /// suite (`this_suite_cannot_reach_the_live_amux_home`).
    fn repo_registry() -> Vec<toml::Table> {
        boundaries::layer(boundaries::DEFAULT_REGISTRY)
            .expect("the committed boundaries.toml parses")
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

    /// A rule with no predicate beyond `tools` applies to NOTHING — the same
    /// answer the guard gives (`rule_applies`: a Bash rule needs a non-empty
    /// `match_all`, a file rule needs `path`). This used to refuse the tool
    /// outright, which is the mirror image of the review's bug: the registry
    /// states what to refuse; a rule that states no predicate states nothing.
    #[test]
    fn a_rule_with_no_predicate_refuses_nothing_like_the_guard() {
        let text = r#"
[[boundary]]
id = "no-predicate"
[[boundary.deny]]
tools = ["Bash", "Write"]
reason = "this rule states no predicate at all"
"#;
        let t: toml::Table = toml::from_str(text).unwrap();
        let reg: Vec<toml::Table> = t
            .get("boundary")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect())
            .unwrap();
        let policy = BoundaryPolicy::from_registry(reg);
        for a in [ask("bash", &[]), ask("write", &["/home/pv/.amux/vault/x"])] {
            let d = policy.decide_detailed(&a);
            assert_eq!(d.approval, Approval::AllowOnce, "{a:?}: {d:?}");
            assert_eq!(d.refused_by, None, "{a:?}: {d:?}");
        }
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
    /// about to be written, measured as a unified diff. It is a LIST in the
    /// registry, and the rule also states `path`, exactly like the real
    /// `boundaries-self` rule: a content predicate narrows a path, it never
    /// replaces one.
    #[test]
    fn an_edit_ask_is_refused_by_a_content_rule_when_the_patch_carries_it() {
        let text = r#"
[[boundary]]
id = "secrets"
[[boundary.deny]]
tools = ["Write", "Edit"]
path = '(^|/)\.claude/settings(\.local)?\.json$'
content_any = ['"hooks"|boundary-guard|disableAllHooks']
reason = "a live secret must never be written to a file"
"#;
        let parsed: toml::Table = toml::from_str(text).unwrap();
        let reg: Vec<toml::Table> = parsed.get("boundary").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect()).unwrap();
        let policy = BoundaryPolicy::from_registry(reg);
        let settings = "/Users/ethan/.claude/settings.json";
        let patch = "Index: settings.json\n@@ -0,0 +1,1 @@\n+{\"hooks\": {}}\n";
        let d = policy.decide_detailed(&ask_with_input("edit", &[settings], patch));
        assert_eq!(d.approval, Approval::Reject, "the written text must be refused: {d:?}");

        // The registry's own allow example: same path, text the rule does not
        // name. The path alone is NOT enough to refuse — that was the review.
        let ok = policy.decide_detailed(&ask_with_input("edit", &[settings], "{\"permissions\": {}}"));
        assert_eq!(ok.approval, Approval::AllowOnce, "{ok:?}");

        // And the same text on a path the rule does not name is untouched.
        let elsewhere = policy.decide_detailed(&ask_with_input(
            "edit",
            &["/Users/ethan/Dev/amux/docs/settings.md"],
            patch,
        ));
        assert_eq!(elsewhere.approval, Approval::AllowOnce, "{elsewhere:?}");
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

    /// Replay one registry example the way the guard would see it: a
    /// `Tool /path[ :: content]` example is a FILE ask, everything else is a
    /// `shell` ask carrying its command (measured, 2.2.4: the command is in
    /// `resources[0]`, which an ask carries as its location and input).
    fn replay_example(ex: &str) -> PermissionAsk {
        for tool in ["Write", "Edit", "MultiEdit", "Read", "Grep", "Glob"] {
            let Some(rest) = ex.strip_prefix(&format!("{tool} ")) else {
                continue;
            };
            let (path, content) = match rest.split_once(" :: ") {
                Some((p, c)) => (p.trim(), Some(c.to_string())),
                None => (rest.trim(), None),
            };
            return PermissionAsk {
                tool: tool.to_ascii_lowercase(),
                title: Some(path.to_string()),
                kind: Some("tool".into()),
                locations: vec![path.to_string()],
                input: content,
                option_ids: vec!["once".into(), "always".into(), "reject".into()],
            };
        }
        ask_with_input("shell", &[ex], ex)
    }

    /// The parity corpus, BOTH directions: every `block_examples` entry must be
    /// refused and every `allow_examples` entry must survive, replayed against
    /// the WHOLE registry (as the guard does — an allow example must not be
    /// refused by any boundary).
    ///
    /// This is the test the review asked for in the other direction: the
    /// evaluator used to refuse an allow example as soon as a rule's `path`
    /// matched a location, which a `shell` ask's command always does when it
    /// merely NAMES a guarded path. One rule is excluded, named below, because
    /// what decides it is a gate receipt and not anything an ask carries.
    #[test]
    fn the_registry_examples_agree_with_this_evaluator_in_both_directions() {
        let registry = repo_registry();
        let policy = BoundaryPolicy::from_registry(registry.clone());
        let (mut refused, mut allowed) = (0, 0);
        let mut receipt_gated = std::collections::BTreeSet::new();
        for boundary in registry.iter() {
            let id = boundary.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let Some(rules) = boundary.get("deny").and_then(|v| v.as_array()) else {
                continue;
            };
            for rule in rules.iter().filter_map(|r| r.as_table()) {
                // A receipt-gated rule is decided by the guard's receipt store,
                // per command segment; an ask carries no receipt. It must never
                // REFUSE from here (that is what broke the allow examples).
                let gated = rule.contains_key("requires_receipt");
                for (key, want_refusal) in
                    [("block_examples", true), ("allow_examples", false)]
                {
                    for ex in rule
                        .get(key)
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str())
                    {
                        let d = policy.decide_detailed(&replay_example(ex));
                        if gated {
                            receipt_gated.insert(id.to_string());
                            assert_eq!(
                                d.approval,
                                Approval::AllowOnce,
                                "boundary {id}: a receipt-gated rule must never refuse an ask \
                                 it cannot see a receipt for; {key} example {ex}"
                            );
                            continue;
                        }
                        if want_refusal {
                            assert_eq!(
                                d.approval,
                                Approval::Reject,
                                "boundary {id}: its own block_example {ex} must be refused by \
                                 this evaluator (path/tool: {d:?})"
                            );
                            refused += 1;
                        } else {
                            assert_eq!(
                                d.approval,
                                Approval::AllowOnce,
                                "boundary {id}: its own allow_example {ex} must NOT be refused, \
                                 but {:?} refused it",
                                d.refused_by
                            );
                            allowed += 1;
                        }
                    }
                }
            }
        }
        assert!(
            refused >= 20 && allowed >= 20,
            "the corpus must actually be replayed, not skipped: {refused} blocks, {allowed} allows"
        );
        assert_eq!(
            receipt_gated.into_iter().collect::<Vec<_>>(),
            vec!["tenant-workload-gate".to_string()],
            "exactly one boundary's rules need state an ask does not carry; if this set \
             changes, the reason recorded in `decide_detailed` no longer holds"
        );
    }

    /// The review's five false refusals, replayed against the REAL registry:
    /// read-only commands that merely name a guarded path were refused because
    /// a shell ask's command sits in `locations`. They are the registry's own
    /// allow examples, so they are allowed here too.
    #[test]
    fn a_read_only_command_that_names_a_guarded_path_is_allowed() {
        let registry = repo_registry();
        let policy = BoundaryPolicy::from_registry(registry);
        for cmd in [
            "ls ~/.amux/gmail-tokens",
            "cat ~/.amux/email-approvals/apr_1.approved.json",
            "cat ~/.claude/settings.json",
            "grep boundary-guard ~/.claude/settings.json",
            "ls ~/.amux/vault",
        ] {
            let d = policy.decide_detailed(&ask_with_input("shell", &[cmd], cmd));
            assert_eq!(
                d.approval,
                Approval::AllowOnce,
                "{cmd} is read-only and names a guarded path; refused by {:?}",
                d.refused_by
            );
        }
        // The same shape, actually guarded, still refuses.
        let d = policy.decide_detailed(&ask_with_input(
            "shell",
            &["cat ~/.amux/vault/items.json"],
            "cat ~/.amux/vault/items.json",
        ));
        assert_eq!(
            d.approval,
            Approval::Reject,
            "a read of the vault ITEM is still refused: {d:?}"
        );
    }

    /// A receipt-gated rule (tenant-workload-gate) is reported, never decided:
    /// the guard's receipt is what makes it refuse, and an ask carries none.
    #[test]
    fn a_receipt_gated_rule_is_reported_not_decided() {
        let registry = repo_registry();
        let policy = BoundaryPolicy::from_registry(registry);
        for cmd in [
            "kubectl -n mixpeek-tenant-a scale deploy/api --replicas=0",
            "kubectl -n kube-system rollout restart deploy/coredns",
        ] {
            let d = policy.decide_detailed(&ask_with_input("shell", &[cmd], cmd));
            assert_eq!(d.approval, Approval::AllowOnce, "{cmd}: {d:?}");
            assert_eq!(
                d.undecidable,
                vec!["tenant-workload-gate".to_string()],
                "{cmd}: the gap must be named, not silently passed"
            );
        }
    }
}
