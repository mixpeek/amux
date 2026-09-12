//! Minimal adapters for gemini, codex, ollama, and muse (RR-0043).
//!
//! "Minimal" is a statement about USAGE, not a placeholder: none of the three
//! exposes a usage/quota API amux can read on this host today
//! (docs/provider-coverage.csv: rate limits are terminal-scrape-only for all
//! of them), so `usage()` returns [`ProviderUsage::unknown`] — zero windows,
//! headline [`UsageConfidence::Unknown`]. Inventing a number here is exactly
//! what Invariant 20 forbids, and routing (RR-0044) treats Unknown as
//! "never exhausted" rather than guessing.
//!
//! Capability claims below cite the OpenCode spike
//! (docs/opencode-spike-results.md, RR-0028e) — they are measured, not
//! aspirational.

use amux_core::provider::{ProviderCapabilities, ProviderId, ProviderUsage};
use async_trait::async_trait;

use super::{PromptMode, ProviderAdapter};

// ---------------------------------------------------------------------------
// Gemini CLI
// ---------------------------------------------------------------------------

/// Gemini CLI (spike: v0.53.1). `--output-format stream-json` mirrors Claude
/// Code's structured shape; a hooks system exists (`gemini hooks`).
pub struct GeminiAdapter;

#[async_trait]
impl ProviderAdapter for GeminiAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::new("gemini")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            hot_model_switch: false,
            // No readable usage surface (terminal scrape only, per spike).
            reports_usage: false,
            // stream-json per the spike coverage matrix.
            structured_events: true,
            // `gemini hooks` (docs/provider-coverage.csv).
            hooks: true,
        }
    }

    async fn usage(&self) -> ProviderUsage {
        // No usage API to read; unknown is the only honest answer.
        ProviderUsage::unknown(self.id())
    }

    async fn models(&self) -> Vec<String> {
        crate::provider::model_catalog::worker_model_ids("gemini")
    }

    fn build_command(&self, prompt_mode: PromptMode) -> Vec<String> {
        match prompt_mode {
            PromptMode::Interactive => vec!["gemini".into()],
            // Non-interactive when stdin is a pipe; structured events via
            // stream-json (spike: mirrors Claude Code's shape). The prompt
            // arrives over stdin — the protocol's job, not argv's.
            PromptMode::HeadlessStructured => vec![
                "gemini".into(),
                "--output-format".into(),
                "stream-json".into(),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Codex CLI
// ---------------------------------------------------------------------------

/// Codex CLI (spike: v0.141.0). `codex exec --json` emits typed JSONL
/// lifecycle events.
pub struct CodexAdapter;

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::new("codex")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            hot_model_switch: false,
            // Usage-limit messages are terminal text only (spike).
            reports_usage: false,
            // JSONL via `codex exec --json` (spike coverage matrix).
            structured_events: true,
            // Hooks exist (spike: `--dangerously-bypass-hook-trust` for
            // automation).
            hooks: true,
        }
    }

    async fn usage(&self) -> ProviderUsage {
        ProviderUsage::unknown(self.id())
    }

    async fn models(&self) -> Vec<String> {
        // Codex itself has no subscription model-listing command. The dated
        // official fallback is therefore the enumerable surface, while the
        // configured model remains an unrestricted open string.
        crate::provider::model_catalog::worker_model_ids("codex")
    }

    fn build_command(&self, prompt_mode: PromptMode) -> Vec<String> {
        match prompt_mode {
            PromptMode::Interactive => vec!["codex".into()],
            // Typed JSONL events on stdout (spike).
            PromptMode::HeadlessStructured => {
                vec!["codex".into(), "exec".into(), "--json".into()]
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

/// Ollama (spike: v0.20.5) — a local model SERVER, not an agent CLI: no
/// hooks, no structured lifecycle events from the `ollama` binary, no usage
/// accounting (locally unlimited != a number; still Unknown, zero windows).
pub struct OllamaAdapter {
    /// `ollama run <model>` requires a model in argv. This default is
    /// CONFIGURATION, not knowledge — callers should override it from
    /// `WorkerConfig.model`; the default only keeps a bare adapter runnable.
    pub default_model: String,
}

/// Compiled-in fallback when nothing is configured. It is a HINT, not a claim
/// that this model exists — see [`ollama_default_model`].
const OLLAMA_FALLBACK_MODEL: &str = "qwen3.8:27b";

/// The ollama model to launch when the caller names none.
///
/// # Why this is a knob and not a literal
///
/// This was hardcoded to `qwen3.8:27b` in TWO places (here and
/// `session_verbs::default_model_for_provider`), and the second one's comment
/// said the quiet part out loud: "a launchable default is required (this box
/// has qwen3.8:27b pulled)". A fact about ONE machine, compiled into a public
/// OSS server — so every other install's ollama default names a 17 GB model
/// they have never pulled, and the two copies could drift from each other
/// besides.
///
/// It surfaced from the other end (DESKT-6): deleting that model on the box
/// that does have it would have broken the default for every future ollama
/// worker, which turned a `ollama rm` into a code change.
///
/// This is the shape `ethos.md` D3 already settled for the helper tier —
/// "One knob: `AMUX_HELPER_MODEL`; all sites read it… the helper tier moves
/// with one line of config". Same answer here, same reason: a pinned model is
/// a bet that cannot improve, and the fix is to make it configuration rather
/// than to pick a different literal.
pub fn ollama_default_model() -> String {
    std::env::var("AMUX_OLLAMA_DEFAULT_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| OLLAMA_FALLBACK_MODEL.to_string())
}

impl Default for OllamaAdapter {
    fn default() -> Self {
        Self {
            default_model: ollama_default_model(),
        }
    }
}

impl OllamaAdapter {
    pub fn with_model(model: impl Into<String>) -> Self {
        Self {
            default_model: model.into(),
        }
    }
}

#[async_trait]
impl ProviderAdapter for OllamaAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::new("ollama")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // ollama workers now run codex --oss --local-provider ollama, which
        // emits structured JSONL events identical to the codex provider.
        ProviderCapabilities {
            structured_events: true,
            hooks: true,
            reports_usage: false,
            hot_model_switch: false,
        }
    }

    async fn usage(&self) -> ProviderUsage {
        // "Effectively unlimited" is not a number amux may invent; the
        // absence of windows says exactly what is known: nothing.
        ProviderUsage::unknown(self.id())
    }

    async fn models(&self) -> Vec<String> {
        // The one place a live listing exists: the local `ollama list`
        // subprocess. Binary missing / daemon down / timeout -> empty vec,
        // the honest answer on a host without ollama.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new("ollama").arg("list").output(),
        )
        .await;
        match out {
            Ok(Ok(o)) if o.status.success() => {
                parse_ollama_list(&String::from_utf8_lossy(&o.stdout))
            }
            _ => Vec::new(),
        }
    }

    fn build_command(&self, prompt_mode: PromptMode) -> Vec<String> {
        // ollama workers run codex with --oss --local-provider ollama so they
        // get a full coding agent (file editing, hooks, structured events) backed
        // by the local Ollama daemon instead of the OpenAI API.
        //
        // -a never: amux workers are autonomous; don't prompt for every shell
        //   command approval.
        // --sandbox workspace-write: codex defaults to read-only sandbox, so
        //   file writes are OS-blocked without this flag. Explicit here so both
        //   the herdr/bootstrap path (which calls this directly) and the tmux
        //   path (session_verbs.rs, which guards on !opts.contains) agree.
        match prompt_mode {
            PromptMode::Interactive => vec![
                "codex".into(),
                "--oss".into(),
                "--local-provider".into(),
                "ollama".into(),
                "--model".into(),
                self.default_model.clone(),
                "-a".into(),
                "never".into(),
                "--sandbox".into(),
                "workspace-write".into(),
                // Local models don't support extended thinking (xhigh). The
                // global ~/.codex/config.toml may set model_reasoning_effort=xhigh
                // for OpenAI models; override it here so ollama workers use low
                // effort and are responsive (xhigh hangs qwen, ~30min wasted: AH-81).
                "-c".into(),
                "model_reasoning_effort=low".into(),
            ],
            PromptMode::HeadlessStructured => vec![
                "codex".into(),
                "--oss".into(),
                "--local-provider".into(),
                "ollama".into(),
                "--model".into(),
                self.default_model.clone(),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Muse Code
// ---------------------------------------------------------------------------

/// Meta's Muse Code CLI (`muse`, measured against 1.0.3-R2198.1 on macOS).
///
/// HOOKS ARE TRUE HERE, and that is the row that matters. Every other non-Claude
/// adapter reports `hooks: false` because its CLI has no hook surface at all —
/// docs/provider-parity.md row 11 records that as a GAP (upstream) for Gemini,
/// with terminal scraping as the sanctioned fallback per D1. Muse ships the
/// SAME contract Claude Code does: the nine events (UserPromptSubmit,
/// PreToolUse, PostToolUse, SessionStart, SessionEnd, Stop, SubagentStop,
/// Notification, PreCompact), the `hookSpecificOutput.hookEventName` envelope,
/// and a `$HOME/.config/muse/settings.json` to declare them in. Verified by
/// inspecting the shipped binary, not from documentation.
///
/// So a muse lane can self-report through the D1 path instead of being scraped,
/// which is the difference between board attribution that is TOLD what happened
/// and board attribution that infers it from pixels. Wiring those hooks is a
/// separate change; this flag is what tells the rest of amux it is possible.
///
/// No usage/quota API on this host, so `usage()` stays Unknown (Invariant 20).
pub struct MuseAdapter;

#[async_trait]
impl ProviderAdapter for MuseAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::new("muse")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            hot_model_switch: false,
            reports_usage: false,
            // `muse exec --json` emits machine-readable JSONL events.
            structured_events: true,
            // See the doc comment: Claude-Code-parity hook contract, verified.
            hooks: true,
        }
    }

    async fn usage(&self) -> ProviderUsage {
        ProviderUsage::unknown(self.id())
    }

    async fn models(&self) -> Vec<String> {
        // From the live provider catalog written by the CLI itself
        // (~/.local/share/muse/model-catalog/*.json): 1.3-contributor carries
        // is_default/is_current, so it leads. Ordered newest-first, not
        // alphabetically — the picker shows this order.
        vec![
            "muse-spark-1.3-contributor".into(),
            "muse-spark-1.3".into(),
            "muse-spark-1.2".into(),
        ]
    }

    fn build_command(&self, prompt_mode: PromptMode) -> Vec<String> {
        match prompt_mode {
            PromptMode::Interactive => vec!["muse".into()],
            // `exec` is the headless verb; `--json` is what makes it structured.
            // Both are required — bare `muse --json` is not a thing.
            PromptMode::HeadlessStructured => {
                vec!["muse".into(), "exec".into(), "--json".into()]
            }
        }
    }
}

/// Parse `ollama list` output: a header line, then one model per line with
/// the name as the first whitespace-separated column, e.g.
/// `llama3:latest    365c0bd3c000    4.7 GB    2 weeks ago`.
fn parse_ollama_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip(1) // "NAME  ID  SIZE  MODIFIED"
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| name.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::provider::UsageConfidence;

    #[tokio::test]
    async fn static_usage_is_unknown_with_zero_windows() {
        for adapter in [
            &GeminiAdapter as &dyn ProviderAdapter,
            &CodexAdapter,
            &OllamaAdapter::default(),
            &MuseAdapter,
        ] {
            let usage = adapter.usage().await;
            assert_eq!(usage.provider, adapter.id());
            assert!(
                usage.windows.is_empty(),
                "{}: minimal adapter must report zero windows",
                adapter.id()
            );
            assert_eq!(usage.confidence(), UsageConfidence::Unknown);
        }
    }

    #[test]
    fn capability_claims_match_the_spike() {
        assert!(GeminiAdapter.capabilities().structured_events);
        assert!(!GeminiAdapter.capabilities().reports_usage);
        assert!(CodexAdapter.capabilities().structured_events);
        assert!(!CodexAdapter.capabilities().reports_usage);
        let ollama = OllamaAdapter::default().capabilities();
        // ollama now uses codex --oss --local-provider ollama, so it inherits
        // codex's structured-event and hook capabilities.
        assert!(ollama.structured_events);
        assert!(ollama.hooks);
        assert!(!ollama.reports_usage);
        assert!(!ollama.hot_model_switch);
        // Muse is the ONLY non-Claude adapter claiming hooks. If this assert is
        // ever "fixed" by flipping it to false, docs/provider-parity.md row 11
        // silently regresses from MET to a scrape fallback for muse lanes.
        let muse = MuseAdapter.capabilities();
        assert!(muse.hooks, "muse ships the Claude-parity hook contract");
        assert!(muse.structured_events);
        assert!(!muse.reports_usage);
        assert!(!muse.hot_model_switch);
    }

    #[test]
    fn muse_builds_muse_command() {
        assert_eq!(MuseAdapter.build_command(PromptMode::Interactive), vec!["muse"]);
        // `exec` AND `--json`: bare `muse --json` is not a valid invocation.
        assert_eq!(
            MuseAdapter.build_command(PromptMode::HeadlessStructured),
            vec!["muse", "exec", "--json"]
        );
    }

    #[tokio::test]
    async fn muse_models_lead_with_the_catalog_default() {
        let m = MuseAdapter.models().await;
        assert_eq!(m.first().map(String::as_str), Some("muse-spark-1.3-contributor"));
        assert!(m.iter().any(|x| x == "muse-spark-1.2"));
    }

    #[test]
    fn ollama_builds_codex_oss_command() {
        let a = OllamaAdapter::with_model("qwen3.8:27b");
        // Interactive: includes -a never + --sandbox workspace-write so both the
        // herdr/bootstrap path (calls this directly) and the tmux path (which
        // guards on !opts.contains) launch with file-editing and no approval prompts.
        let interactive_expected = vec![
            "codex", "--oss", "--local-provider", "ollama", "--model", "qwen3.8:27b",
            "-a", "never", "--sandbox", "workspace-write",
            "-c", "model_reasoning_effort=low",
        ];
        // HeadlessStructured: no extra flags needed (headless driver handles approvals).
        let headless_expected = vec![
            "codex", "--oss", "--local-provider", "ollama", "--model", "qwen3.8:27b",
        ];
        assert_eq!(a.build_command(PromptMode::Interactive), interactive_expected);
        assert_eq!(a.build_command(PromptMode::HeadlessStructured), headless_expected);
    }

    #[test]
    fn codex_headless_is_exec_json() {
        assert_eq!(
            CodexAdapter.build_command(PromptMode::HeadlessStructured),
            vec!["codex", "exec", "--json"]
        );
    }

    #[test]
    fn parses_ollama_list_output() {
        let fixture = "NAME               ID              SIZE      MODIFIED\n\
                       llama3:latest      365c0bd3c000    4.7 GB    2 weeks ago\n\
                       qwen3.8:27b           500a1f067a9f    5.2 GB    3 days ago\n";
        assert_eq!(
            parse_ollama_list(fixture),
            vec!["llama3:latest", "qwen3.8:27b"]
        );
        assert!(parse_ollama_list("").is_empty());
        assert!(parse_ollama_list("NAME  ID  SIZE  MODIFIED\n").is_empty());
    }
}
#[cfg(test)]
mod ollama_default_tests {
    use super::*;

    /// These tests mutate ONE process-wide env var, so run in parallel they
    /// clobber each other: measured 4 failures in 5 runs before this guard.
    /// A flaky test is worse than no test — it trains people to re-run rather
    /// than to read. `--test-threads=1` would hide it here and still flake in
    /// CI, so the serialization belongs in the test, not in how it is invoked.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Sets the var (or clears it), runs `f`, and always restores — including
    /// on panic, since a leaked value would silently corrupt whichever test
    /// grabs the lock next.
    fn with_knob<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        // Poisoning is irrelevant here: a panicking test still restored its
        // value via the guard below, so the env is clean either way.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AMUX_OLLAMA_DEFAULT_MODEL").ok();
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: ENV_LOCK is held for the whole scope, so no other
                // test in this binary is reading or writing this var.
                match &self.0 {
                    Some(p) => unsafe { std::env::set_var("AMUX_OLLAMA_DEFAULT_MODEL", p) },
                    None => unsafe { std::env::remove_var("AMUX_OLLAMA_DEFAULT_MODEL") },
                }
            }
        }
        let _restore = Restore(prev);
        // SAFETY: as above — the lock makes this the only writer.
        match value {
            Some(v) => unsafe { std::env::set_var("AMUX_OLLAMA_DEFAULT_MODEL", v) },
            None => unsafe { std::env::remove_var("AMUX_OLLAMA_DEFAULT_MODEL") },
        }
        f()
    }

    /// The guard has to actually serialize, or every assertion below is
    /// satisfied by luck. Two threads hammering opposite values must never see
    /// the other's.
    #[test]
    fn the_env_guard_actually_serializes() {
        std::thread::scope(|sc| {
            for (val, expect) in [("model-a", "model-a"), ("model-b", "model-b")] {
                sc.spawn(move || {
                    for _ in 0..200 {
                        with_knob(Some(val), || assert_eq!(ollama_default_model(), expect));
                    }
                });
            }
        });
    }

    /// The two sites must not be able to disagree. They WERE two literals, and
    /// "a view must share the predicate of the mechanism it describes" is
    /// exactly what stops the next person changing one of them.
    #[test]
    fn the_adapter_and_the_launcher_resolve_the_same_model() {
        with_knob(Some("pinned:test"), || {
            assert_eq!(OllamaAdapter::default().default_model, ollama_default_model());
            assert_eq!(OllamaAdapter::default().default_model, "pinned:test");
        });
    }

    /// Unset must preserve TODAY's behaviour exactly — this change is meant to
    /// make the default movable, not to move it.
    #[test]
    fn unset_keeps_the_compiled_fallback() {
        with_knob(None, || assert_eq!(ollama_default_model(), OLLAMA_FALLBACK_MODEL));
    }

    /// The knob has to actually move it, or it is decoration. This is the
    /// assertion that makes `ollama rm qwen3.8:27b` a config change rather than
    /// a code change (DESKT-6).
    #[test]
    fn the_knob_moves_the_default() {
        with_knob(Some("qwen2.5vl:3b"), || {
            assert_eq!(ollama_default_model(), "qwen2.5vl:3b");
            assert_eq!(
                OllamaAdapter::default().default_model,
                "qwen2.5vl:3b",
                "the adapter must follow the knob too, not just the launcher"
            );
        });
    }

    /// An empty or whitespace-only value is a mis-set knob, not a request for
    /// an empty model name — which would build `--model ` and fail obscurely
    /// at launch instead of here.
    #[test]
    fn a_blank_knob_falls_back_rather_than_launching_an_empty_model() {
        for blank in ["", "   "] {
            with_knob(Some(blank), || {
                assert_eq!(ollama_default_model(), OLLAMA_FALLBACK_MODEL, "blank {blank:?}");
            });
        }
    }
}
