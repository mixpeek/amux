//! Minimal adapters for gemini, codex, ollama, and grok (RR-0043).
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
        // The CLI's own selectable tiers; no listing endpoint to query.
        vec!["gemini-2.5-pro".into(), "gemini-2.5-flash".into()]
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
        // No enumerable model surface from the CLI; empty is honest — the
        // configured model rides in WorkerConfig, not here.
        Vec::new()
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

impl Default for OllamaAdapter {
    fn default() -> Self {
        Self {
            default_model: "qwen3.8:27b".into(),
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
// Grok Build
// ---------------------------------------------------------------------------

/// Grok Build CLI (`grok`). Interactive TUI; headless structured events via
/// `--output-format streaming-json`. No usage/quota API today — `usage()`
/// returns unknown (Invariant 20). Hooks are not advertised until a hook
/// surface exists on the CLI.
pub struct GrokAdapter;

#[async_trait]
impl ProviderAdapter for GrokAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::new("grok")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            hot_model_switch: false,
            reports_usage: false,
            // `--output-format streaming-json` emits NDJSON ACP session updates.
            structured_events: true,
            hooks: false,
        }
    }

    async fn usage(&self) -> ProviderUsage {
        ProviderUsage::unknown(self.id())
    }

    async fn models(&self) -> Vec<String> {
        vec!["grok-4.6".into(), "grok-4.5".into()]
    }

    fn build_command(&self, prompt_mode: PromptMode) -> Vec<String> {
        match prompt_mode {
            PromptMode::Interactive => vec!["grok".into()],
            PromptMode::HeadlessStructured => vec![
                "grok".into(),
                "--output-format".into(),
                "streaming-json".into(),
            ],
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
            &GrokAdapter,
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
        let grok = GrokAdapter.capabilities();
        assert!(grok.structured_events);
        assert!(!grok.hooks);
        assert!(!grok.reports_usage);
        assert!(!grok.hot_model_switch);
    }

    #[test]
    fn grok_builds_grok_command() {
        assert_eq!(
            GrokAdapter.build_command(PromptMode::Interactive),
            vec!["grok"]
        );
        assert_eq!(
            GrokAdapter.build_command(PromptMode::HeadlessStructured),
            vec!["grok", "--output-format", "streaming-json"]
        );
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
