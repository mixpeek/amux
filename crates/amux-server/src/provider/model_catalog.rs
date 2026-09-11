//! Typed model catalog shared by provider adapters and the dashboard.
//!
//! The dashboard used to carry three independent arrays of model ids. That
//! made every new provider release a partial UI update: a model could appear
//! in one settings picker while remaining absent from a worker's picker. This
//! module is the single compiled-in fallback. The open `WorkerConfig.model`
//! string remains the authority, so an id released after this catalog date can
//! still be entered immediately instead of waiting for an amux release.
//!
//! Sources checked 2026-09-06:
//! - <https://developers.openai.com/api/docs/models/all>
//! - <https://platform.claude.com/docs/en/models/overview>
//! - <https://ai.google.dev/gemini-api/docs/models>

use serde::Serialize;

pub const CATALOG_UPDATED_AT: &str = "2026-09-08";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelDescriptor {
    /// Vendor that publishes the model (distinct from the amux CLI provider).
    pub vendor: &'static str,
    /// amux provider id used by workers and dashboard controls.
    pub provider: &'static str,
    /// Exact model id or provider-supported alias.
    pub id: &'static str,
    /// Capability/family grouping used by the UI and filters.
    pub model_type: &'static str,
    /// Whether this is meaningful as a coding-agent CLI's `--model` value.
    pub worker_selectable: bool,
}

fn add(
    out: &mut Vec<ModelDescriptor>,
    vendor: &'static str,
    provider: &'static str,
    model_type: &'static str,
    worker_selectable: bool,
    ids: &'static [&'static str],
) {
    out.extend(ids.iter().map(|id| ModelDescriptor {
        vendor,
        provider,
        id,
        model_type,
        worker_selectable,
    }));
}

/// Every model exposed by the official OpenAI, Gemini and Muse catalogs, plus
/// the current/legacy Claude ids and provider aliases already supported by amux.
/// Non-agent modalities are represented (and typed) but deliberately not
/// offered to coding workers.
pub fn catalog() -> Vec<ModelDescriptor> {
    let mut out = Vec::new();

    add(
        &mut out,
        "openai",
        "codex",
        "flagship",
        true,
        &[
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.4",
            "gpt-5.4-pro",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.2",
            "gpt-5.2-pro",
            "gpt-5.1",
            "gpt-5",
            "gpt-5-pro",
            "gpt-5-mini",
            "gpt-5-nano",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "coding",
        true,
        &[
            "gpt-5.3-codex",
            // Codex subscription id: supported by the CLI but not listed as a
            // public API model page.
            "gpt-5.3-codex-spark",
            "gpt-5.2-codex",
            "gpt-5.1-codex",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex-mini",
            "gpt-5-codex",
            "codex-mini-latest",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "reasoning",
        true,
        &[
            "o1",
            "o1-mini",
            "o1-preview",
            "o1-pro",
            "o3",
            "o3-mini",
            "o3-pro",
            "o4-mini",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "general",
        true,
        &[
            "chat-latest",
            "chatgpt-4o-latest",
            "gpt-5-chat-latest",
            "gpt-5.1-chat-latest",
            "gpt-5.2-chat-latest",
            "gpt-5.3-chat-latest",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-4.5-preview",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4",
            "gpt-4-turbo",
            "gpt-4-turbo-preview",
            "gpt-3.5-turbo",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "research",
        true,
        &["o3-deep-research", "o4-mini-deep-research"],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "search",
        true,
        &["gpt-4o-search-preview", "gpt-4o-mini-search-preview"],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "computer-use",
        true,
        &["computer-use-preview"],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "cybersecurity",
        true,
        &[
            "gpt-5.6-cyber",
            "gpt-daybreak-red-latest",
            "gpt-daybreak-blue-latest",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "open-weight",
        true,
        &["gpt-oss-120b", "gpt-oss-20b"],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "legacy-completion",
        false,
        &["babbage-002", "davinci-002"],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "image",
        false,
        &[
            "chatgpt-image-latest",
            "gpt-image-2",
            "gpt-image-1.5",
            "gpt-image-1",
            "gpt-image-1-mini",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "audio",
        false,
        &[
            "gpt-audio",
            "gpt-audio-1.5",
            "gpt-audio-mini",
            "gpt-4o-audio-preview",
            "gpt-4o-mini-audio-preview",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "realtime",
        false,
        &[
            "gpt-realtime",
            "gpt-realtime-1.5",
            "gpt-realtime-2",
            "gpt-realtime-2.1",
            "gpt-realtime-2.1-mini",
            "gpt-realtime-mini",
            "gpt-realtime-translate",
            "gpt-realtime-whisper",
            "gpt-4o-realtime-preview",
            "gpt-4o-mini-realtime-preview",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "transcription",
        false,
        &[
            "gpt-live-transcribe",
            "gpt-transcribe",
            "gpt-4o-transcribe",
            "gpt-4o-mini-transcribe",
            "gpt-4o-transcribe-diarize",
            "whisper-1",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "speech",
        false,
        &["gpt-4o-mini-tts", "tts-1", "tts-1-hd"],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "embedding",
        false,
        &[
            "text-embedding-3-large",
            "text-embedding-3-small",
            "text-embedding-ada-002",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "moderation",
        false,
        &[
            "omni-moderation-latest",
            "text-moderation-latest",
            "text-moderation-stable",
        ],
    );
    add(
        &mut out,
        "openai",
        "codex",
        "video",
        false,
        &["sora-2", "sora-2-pro"],
    );

    add(
        &mut out,
        "anthropic",
        "claude",
        "alias",
        true,
        &["opus", "sonnet", "haiku"],
    );
    add(
        &mut out,
        "anthropic",
        "claude",
        "fable",
        true,
        &["claude-fable-5-1", "claude-fable-5"],
    );
    add(
        &mut out,
        "anthropic",
        "claude",
        "opus",
        true,
        &[
            "claude-opus-5",
            "claude-opus-5[1m]",
            "claude-opus-4-8",
            "claude-opus-4-8[1m]",
            "claude-opus-4-7",
            "claude-opus-4-7[1m]",
            "claude-opus-4-6",
            "claude-opus-4-6[1m]",
            "claude-opus-4-5",
            "claude-opus-4-1",
            "claude-opus-4",
            "claude-opus-3",
        ],
    );
    add(
        &mut out,
        "anthropic",
        "claude",
        "sonnet",
        true,
        &[
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-6[1m]",
            "claude-sonnet-4-5",
            "claude-sonnet-4",
            "claude-sonnet-3-7",
            "claude-sonnet-3-5",
        ],
    );
    add(
        &mut out,
        "anthropic",
        "claude",
        "haiku",
        true,
        &[
            "claude-haiku-4-5-20251001",
            "claude-haiku-3-5",
            "claude-haiku-3",
        ],
    );

    add(
        &mut out,
        "google",
        "gemini",
        "alias",
        true,
        &["auto", "gemini-flash-latest"],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "pro",
        true,
        &[
            "gemini-3.1-pro-preview",
            "gemini-3-pro-preview",
            "gemini-2.5-pro",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "flash",
        true,
        &[
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-flash",
            "gemini-3-flash-preview",
            "gemini-2.5-flash",
            "gemini-2.5-flash-preview-09-2025",
            "gemini-2.0-flash",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "flash-lite",
        true,
        &[
            "gemini-3.5-flash-lite",
            "gemini-3.1-flash-lite",
            "gemini-3.1-flash-lite-preview",
            "gemini-2.5-flash-lite",
            "gemini-2.5-flash-lite-preview-09-2025",
            "gemini-2.0-flash-lite",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "computer-use",
        true,
        &["gemini-2.5-computer-use-preview-10-2025"],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "image",
        false,
        &[
            "gemini-3-pro-image",
            "gemini-3.1-flash-image",
            "gemini-3.1-flash-lite-image",
            "gemini-2.5-flash-image",
            "imagen-4.0-generate",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "video",
        false,
        &[
            "gemini-omni-1.1-flash",
            "gemini-omni-flash",
            "veo-3.1-generate-preview",
            "veo-3.1-lite-generate-preview",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "music",
        false,
        &[
            "lyria-3.5",
            "lyria-3-clip-preview",
            "lyria-3-pro-preview",
            "lyria-realtime-exp",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "research-agent",
        false,
        &[
            "deep-research-preview-04-2026",
            "deep-research-max-preview-04-2026",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "managed-agent",
        false,
        &["antigravity-preview-05-2026"],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "live-audio",
        false,
        &[
            "gemini-3.5-live-translate-preview",
            "gemini-3.1-flash-live-preview",
            "gemini-2.5-flash-native-audio-preview-12-2025",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "speech",
        false,
        &[
            "gemini-3.1-flash-tts-preview",
            "gemini-2.5-flash-preview-tts",
            "gemini-2.5-pro-preview-tts",
        ],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "transcription",
        false,
        &["gemini-3.5-transcribe", "gemini-3.5-transcribe-live"],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "embedding",
        false,
        &["gemini-embedding-2-preview", "gemini-embedding-001"],
    );
    add(
        &mut out,
        "google",
        "gemini",
        "robotics",
        false,
        &[
            "gemini-robotics-er-2-preview",
            "gemini-robotics-er-2-streaming-preview",
            "gemini-robotics-er-1.6-preview",
            "gemini-robotics-er-1.5-preview",
        ],
    );

    // Meta / Muse Code. Ids and the default come from the catalog the CLI itself
    // writes (~/.local/share/muse/model-catalog/*.json), where
    // muse-spark-1.3-contributor carries is_default AND is_current — so it leads
    // here too, and `providerDefaultModel` in app.js names the same id. All three
    // are coding models, so all three are worker_selectable.
    add(
        &mut out,
        "meta",
        "muse",
        "spark",
        true,
        &[
            "muse-spark-1.3-contributor",
            "muse-spark-1.3",
            "muse-spark-1.2",
        ],
    );

    out
}

/// Static worker-capable ids for one amux provider. Unknown providers return
/// no guesses; callers still accept an explicit open-string model id.
pub fn worker_model_ids(provider: &str) -> Vec<String> {
    let provider = if provider == "claude-code" {
        "claude"
    } else {
        provider
    };
    catalog()
        .into_iter()
        .filter(|m| m.provider == provider && m.worker_selectable)
        .map(|m| m.id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn every_entry_is_typed_unique_and_attributed() {
        let models = catalog();
        let mut seen = BTreeSet::new();
        for model in &models {
            assert!(!model.vendor.is_empty(), "{} has no vendor", model.id);
            assert!(!model.provider.is_empty(), "{} has no provider", model.id);
            assert!(
                !model.model_type.is_empty(),
                "{} has no model type",
                model.id
            );
            assert!(
                seen.insert((model.provider, model.id)),
                "duplicate model id for {}: {}",
                model.provider,
                model.id
            );
        }

        let counts = models.iter().fold(BTreeMap::new(), |mut counts, model| {
            *counts.entry(model.vendor).or_insert(0usize) += 1;
            counts
        });
        assert!(
            counts["openai"] >= 95,
            "OpenAI catalog unexpectedly shrank: {counts:?}"
        );
        assert!(
            counts["anthropic"] >= 20,
            "Claude catalog unexpectedly shrank: {counts:?}"
        );
        assert!(
            counts["google"] >= 49,
            "Gemini catalog unexpectedly shrank: {counts:?}"
        );
    }

    #[test]
    fn current_flagships_and_every_modality_are_represented() {
        let models = catalog();
        for id in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-image-2",
            "gpt-realtime-2.1",
            "text-embedding-3-large",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-sonnet-5",
            "gemini-3.8-flash",
            "gemini-3.1-pro-preview",
            "gemini-embedding-2-preview",
            "veo-3.1-generate-preview",
            "deep-research-max-preview-04-2026",
        ] {
            assert!(models.iter().any(|model| model.id == id), "missing {id}");
        }
        for model_type in [
            "flagship",
            "coding",
            "reasoning",
            "image",
            "audio",
            "realtime",
            "transcription",
            "speech",
            "embedding",
            "moderation",
            "video",
        ] {
            assert!(
                models.iter().any(|model| model.model_type == model_type),
                "missing model type {model_type}"
            );
        }
    }

    #[test]
    fn non_agent_modalities_never_reach_worker_pickers() {
        for model in catalog() {
            if matches!(
                model.model_type,
                "image"
                    | "audio"
                    | "realtime"
                    | "transcription"
                    | "speech"
                    | "embedding"
                    | "moderation"
                    | "video"
                    | "music"
                    | "research-agent"
                    | "managed-agent"
                    | "robotics"
            ) {
                assert!(
                    !model.worker_selectable,
                    "{} would be offered to a coding worker",
                    model.id
                );
            }
        }
    }
}
