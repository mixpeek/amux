//! ProviderAdapter: the pluggable model-runtime seam (RR-0043, Invariants 8,
//! 20, 21).
//!
//! An adapter answers four questions about one provider: who am I
//! ([`ProviderAdapter::id`]), what can I do ([`ProviderAdapter::capabilities`]),
//! how much capacity is left ([`ProviderAdapter::usage`] — normalized, honest,
//! never invented; Invariant 20), and what argv starts a session
//! ([`ProviderAdapter::build_command`] — the source of
//! `backend::SessionSpec.command`; the backend runs it without interpreting
//! it).
//!
//! Provider-specific logic lives HERE, in per-provider modules — never as
//! `if provider == "gemini"` branches elsewhere (plan §Invariant 8). The
//! registry resolves adapters by open string id, so a provider that does not
//! exist yet registers without recompiling amux-core.
//!
//! Capacity routing (RR-0044) consumes the usage these adapters report; it
//! lives in [`routing`] and is pure — it never talks to an adapter directly,
//! only to the `ProviderUsage` values collected from them.

pub mod claude;
pub mod routing;
pub mod static_providers;

use std::collections::BTreeMap;
use std::sync::Arc;

use amux_core::provider::{ProviderCapabilities, ProviderId, ProviderUsage, UsageConfidence};
use async_trait::async_trait;

/// What kind of session the argv from [`ProviderAdapter::build_command`]
/// starts. Two modes only, because that is the real fork in every provider
/// CLI today (docs/opencode-spike-results.md): a human-attached TTY, or a
/// headless process emitting structured events for the OpenCode protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptMode {
    /// A human attaches a terminal (tmux/herdr window). Interactive UI.
    Interactive,
    /// Headless, driven over stdin/stdout with structured (stream-json/JSONL)
    /// events where the provider supports them — the D1-exit path.
    HeadlessStructured,
}

/// One provider's adapter. `Send + Sync` because the registry shares adapters
/// across the orchestrator's tasks behind `Arc`.
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Open string identity (`"claude-code"`, `"gemini"`, ... — Invariant 8).
    fn id(&self) -> ProviderId;

    /// What this provider can do, so the orchestrator plans around missing
    /// capabilities instead of discovering them as failures.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Normalized best-effort usage (Invariant 20). The contract, enforced by
    /// [`conformance`]: when the adapter cannot know, it returns
    /// [`ProviderUsage::unknown`] / all-`None` windows with
    /// [`UsageConfidence::Unknown`] — NEVER a fabricated number. Read-only:
    /// a usage probe must not mutate provider state.
    async fn usage(&self) -> ProviderUsage;

    /// Models this provider can run. Static lists are legitimate where no
    /// listing interface exists; an empty vec is the honest answer when even
    /// that is unknowable (e.g. ollama with no binary installed).
    async fn models(&self) -> Vec<String>;

    /// The argv that starts a session in the given mode — what the provider
    /// layer hands to `backend::SessionSpec.command`. Never empty.
    fn build_command(&self, prompt_mode: PromptMode) -> Vec<String>;
}

/// Adapters resolved by open string id (Invariant 8): registering a provider
/// nobody has heard of is legal and requires recompiling nothing upstream.
#[derive(Default)]
pub struct ProviderRegistry {
    adapters: BTreeMap<ProviderId, Arc<dyn ProviderAdapter>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) an adapter under its own id.
    pub fn register(&mut self, adapter: Arc<dyn ProviderAdapter>) {
        self.adapters.insert(adapter.id(), adapter);
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn ProviderAdapter>> {
        self.adapters.get(id).cloned()
    }

    /// Resolve a provider id as it appears on WORKER ROWS. Exact match
    /// first; the one legacy spelling — `"claude"`, the `_amux_workers`
    /// schema default (migration 0003) and the create-endpoint default
    /// (api/workers.rs) — resolves to the canonical `"claude-code"`.
    /// Without this alias every default-created worker fails provider
    /// lookup while `GET /api/workers` happily shows `"provider":"claude"`
    /// (two components disagreeing about the same fact; ethos rule 4).
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn ProviderAdapter>> {
        if let Some(a) = self.adapters.get(&ProviderId::new(id)) {
            return Some(a.clone());
        }
        match id {
            "claude" => self.adapters.get(&ProviderId::new("claude-code")).cloned(),
            _ => None,
        }
    }

    pub fn ids(&self) -> Vec<ProviderId> {
        self.adapters.keys().cloned().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ProviderId, &Arc<dyn ProviderAdapter>)> {
        self.adapters.iter()
    }

    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
}

/// The registry every deployment starts from: all known providers,
/// registered by default. Opt-OUT, not opt-in — a capability nobody is
/// enrolled in is decoration (ethos rule 1; the `CC_MCP` incident).
pub fn default_registry() -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register(Arc::new(claude::ClaudeAdapter::new()));
    reg.register(Arc::new(static_providers::GeminiAdapter));
    reg.register(Arc::new(static_providers::CodexAdapter));
    reg.register(Arc::new(static_providers::OllamaAdapter::default()));
    reg.register(Arc::new(static_providers::GrokAdapter));
    reg
}

// ---------------------------------------------------------------------------
// Conformance suite (Invariant 21)
// ---------------------------------------------------------------------------
//
// ONE suite, parameterized over `&dyn ProviderAdapter`, run unchanged against
// every adapter — no assertion below knows which provider it is exercising.
// Split in two so the network-touching half can be gated: `usage()` on a host
// with real credentials performs a real (read-only) probe, which unit tests
// must not do implicitly (the claude full run lives behind #[ignore] in
// claude.rs; CI has no keychain).

/// Synchronous conformance: identity, capabilities, command shape. Panics
/// (naming the provider) on the first violation.
pub fn conformance_static(adapter: &dyn ProviderAdapter) {
    let id = adapter.id();
    assert!(
        !id.as_str().is_empty(),
        "adapter returned an empty ProviderId"
    );

    // capabilities() must return without panic; the value itself is the
    // adapter's claim and is asserted per-provider, not here.
    let _caps = adapter.capabilities();

    for mode in [PromptMode::Interactive, PromptMode::HeadlessStructured] {
        let argv = adapter.build_command(mode);
        assert!(
            !argv.is_empty(),
            "{id}: build_command({mode:?}) returned an empty argv"
        );
        assert!(
            argv.iter().all(|a| !a.is_empty()),
            "{id}: build_command({mode:?}) contains an empty element: {argv:?}"
        );
    }
}

/// Full conformance: everything in [`conformance_static`] plus the usage
/// honesty contract (Invariant 20) and models(). May touch the network /
/// local subprocesses for adapters that really probe — call it accordingly.
pub async fn conformance(adapter: &dyn ProviderAdapter) {
    conformance_static(adapter);

    let id = adapter.id();
    let usage = adapter.usage().await;
    assert_eq!(
        usage.provider, id,
        "{id}: usage() reported usage for a different provider"
    );
    // The honesty contract: an Unknown-confidence window carries NO numbers.
    // "When it cannot know, every field is None" — a fabricated zero under
    // Unknown is exactly the invented number Invariant 20 forbids.
    for w in &usage.windows {
        if w.confidence == UsageConfidence::Unknown {
            assert!(
                w.used.is_none() && w.limit.is_none() && w.resets_at.is_none(),
                "{id}: Unknown-confidence window carries invented numbers: {w:?}"
            );
        }
    }
    // Zero windows must read as Unknown at the headline (amux-core contract).
    if usage.windows.is_empty() {
        assert_eq!(
            usage.confidence(),
            UsageConfidence::Unknown,
            "{id}: empty usage must have Unknown headline confidence"
        );
    }

    // models() must return without panic; empty is legal (honest for a
    // provider whose models cannot be enumerated on this host).
    let _models = adapter.models().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_registers_all_known() {
        let reg = default_registry();
        assert_eq!(reg.len(), 5);
        for id in ["claude-code", "gemini", "codex", "ollama", "grok"] {
            assert!(
                reg.get(&ProviderId::new(id)).is_some(),
                "default registry missing {id}"
            );
        }
    }

    #[test]
    fn registry_is_open_to_unknown_providers() {
        // Invariant 8: a provider from the future registers with no code
        // change anywhere else.
        struct FutureAdapter;
        #[async_trait]
        impl ProviderAdapter for FutureAdapter {
            fn id(&self) -> ProviderId {
                ProviderId::new("a-provider-from-2031")
            }
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn usage(&self) -> ProviderUsage {
                ProviderUsage::unknown(self.id())
            }
            async fn models(&self) -> Vec<String> {
                Vec::new()
            }
            fn build_command(&self, _mode: PromptMode) -> Vec<String> {
                vec!["future".into()]
            }
        }

        let mut reg = default_registry();
        reg.register(Arc::new(FutureAdapter));
        assert_eq!(reg.len(), 6);
        let got = reg.get(&ProviderId::new("a-provider-from-2031")).unwrap();
        assert_eq!(got.id().as_str(), "a-provider-from-2031");
    }

    #[test]
    fn resolve_accepts_the_worker_row_default_spelling() {
        let reg = default_registry();
        // The exact ids all resolve...
        assert_eq!(reg.resolve("claude-code").unwrap().id().as_str(), "claude-code");
        assert_eq!(reg.resolve("gemini").unwrap().id().as_str(), "gemini");
        assert_eq!(reg.resolve("grok").unwrap().id().as_str(), "grok");
        // ...and the schema-default legacy spelling lands on claude-code.
        assert_eq!(reg.resolve("claude").unwrap().id().as_str(), "claude-code");
        assert!(reg.resolve("not-a-provider").is_none());
    }

    #[test]
    fn registry_lookup_by_id_returns_the_right_adapter() {
        let reg = default_registry();
        let claude = reg.get(&ProviderId::new("claude-code")).unwrap();
        assert!(claude.capabilities().hooks);
        let ollama = reg.get(&ProviderId::new("ollama")).unwrap();
        // ollama now runs codex --oss --local-provider ollama, inheriting codex capabilities.
        assert!(ollama.capabilities().hooks);
        assert!(reg.get(&ProviderId::new("not-registered")).is_none());
    }

    // Full conformance for the static adapters: their usage() is local and
    // instant (gemini/codex return unknown; ollama at most runs a local
    // `ollama list`). Claude's full run is #[ignore]d in claude.rs because
    // its usage() performs a real network probe when credentials exist.
    #[tokio::test]
    async fn conformance_gemini() {
        conformance(&static_providers::GeminiAdapter).await;
    }

    #[tokio::test]
    async fn conformance_codex() {
        conformance(&static_providers::CodexAdapter).await;
    }

    #[tokio::test]
    async fn conformance_ollama() {
        conformance(&static_providers::OllamaAdapter::default()).await;
    }

    #[tokio::test]
    async fn conformance_grok() {
        conformance(&static_providers::GrokAdapter).await;
    }

    #[test]
    fn conformance_claude_static() {
        conformance_static(&claude::ClaudeAdapter::new());
    }
}
