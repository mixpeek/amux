//! Live model catalog: periodic, cached fetch of each vendor's own
//! list-models API, merged over the hand-typed static fallback in
//! `model_catalog.rs`.
//!
//! Static entries go stale — that is what `model_catalog.rs`'s own
//! `CATALOG_UPDATED_AT` staleness WARN exists to admit. This module asks the
//! vendor directly whenever a credential is configured, so a model released
//! the same day it ships can appear without an amux code change.
//!
//! NEVER on the request path. `/api/models` and every
//! `ProviderAdapter::models()` read whatever snapshot [`refresh`] last
//! computed; they never block a request on an external GET. A vendor outage
//! or a missing key degrades that ONE vendor back to its static entries —
//! never to an error, and never to erasing an id the static catalog already
//! offered (merge is a union, keyed by id).
//!
//! SECRET DISCIPLINE, same as `claude.rs`'s usage probe: an API key is read,
//! put in one request, and dropped. No [`ProviderStatus`] variant can carry
//! it or a response body — failures record a fixed word or an HTTP status
//! code only.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;

use serde::Serialize;

use super::model_catalog::{self, ModelDescriptor};

/// One budget for every vendor probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Last successful listing per vendor, persisted under the amux home.
///
/// Before this, the live ids lived only in process memory and a failed probe
/// replaced them with nothing. The builder restarts this binary on every
/// commit, so ONE slow first probe after a deploy dropped the pickers back to
/// the compiled-in list until the next hourly tick. Measured 2026-09-26: the
/// 19:20 boot probe timed out on both vendors, `claude-opus-5-5` (live-only,
/// newer than the static catalog) vanished from every model picker, and came
/// back at 20:07. A failed probe now serves this cache instead, marked as such.
const LAST_GOOD_FILE: &str = "model-catalog-live.json";

/// A vendor listed successfully within this window is not asked again. The
/// refresh job ticks more often than this so a FAILED vendor is retried soon,
/// without re-fetching the ones that answered.
pub const FRESH_SECS: i64 = 3600;

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize, PartialEq)]
pub struct LastGood {
    pub ids: Vec<String>,
    pub fetched_at: i64,
}

fn load_last_good(home: &Path) -> BTreeMap<String, LastGood> {
    std::fs::read(home.join(LAST_GOOD_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_last_good(home: &Path, map: &BTreeMap<String, LastGood>) {
    let path = home.join(LAST_GOOD_FILE);
    let tmp = home.join(format!("{LAST_GOOD_FILE}.tmp"));
    let ok = serde_json::to_vec(map)
        .ok()
        .and_then(|b| std::fs::write(&tmp, b).ok())
        .and_then(|()| std::fs::rename(&tmp, &path).ok())
        .is_some();
    if !ok {
        tracing::warn!(kind = "provider_model_catalog_cache_write_failed", path = %path.display(),
            "could not persist the last good model listing; a restart will not be able to reuse it");
    }
}

/// What one vendor contributes to this refresh.
#[derive(Debug, Clone, PartialEq)]
pub enum VendorOutcome {
    /// Listed by this probe.
    Live { ids: Vec<String>, fetched_at: i64 },
    /// The probe failed; serving the last good listing from `fetched_at`.
    Cached { ids: Vec<String>, fetched_at: i64, error: String },
    /// The probe failed and nothing was ever listed.
    Failed(String),
    /// No credential for this vendor.
    Unconfigured,
}

/// Pure decision, so the fallback can be tested without a network.
pub fn decide(prev: Option<&LastGood>, attempt: Option<Result<Vec<String>, String>>, now: i64) -> VendorOutcome {
    match attempt {
        None => VendorOutcome::Unconfigured,
        Some(Ok(ids)) => VendorOutcome::Live { ids, fetched_at: now },
        Some(Err(error)) => match prev {
            Some(p) if !p.ids.is_empty() => VendorOutcome::Cached { ids: p.ids.clone(), fetched_at: p.fetched_at, error },
            _ => VendorOutcome::Failed(error),
        },
    }
}

/// (vendor label, amux provider id, env key that must hold that vendor's key)
/// — the only three vendors with a real list-models HTTP endpoint today.
/// Ollama already lists live via `ollama list` (`static_providers.rs`); Muse
/// already lists live via the CLI's own catalog file. Neither needs an entry
/// here.
const HTTP_VENDORS: [(&str, &str, &str); 3] = [
    ("anthropic", "claude", "ANTHROPIC_API_KEY"),
    ("openai", "codex", "OPENAI_API_KEY"),
    ("google", "gemini", "GEMINI_API_KEY"),
];

/// One catalog entry as served over the wire — owned strings, because a
/// live-fetched id cannot be `&'static str`.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    pub vendor: String,
    pub provider: String,
    pub id: String,
    pub model_type: String,
    pub worker_selectable: bool,
    /// "live" when this id came from the vendor's own listing on the most
    /// recent successful refresh; "cached" when the latest probe failed and
    /// this id is from the last good listing; "static" when it is only known
    /// from the compiled-in fallback.
    pub source: &'static str,
}

impl From<ModelDescriptor> for CatalogEntry {
    fn from(m: ModelDescriptor) -> Self {
        CatalogEntry {
            vendor: m.vendor.to_string(),
            provider: m.provider.to_string(),
            id: m.id.to_string(),
            model_type: m.model_type.to_string(),
            worker_selectable: m.worker_selectable,
            source: "static",
        }
    }
}

/// Per-vendor result of the most recent refresh attempt.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    pub vendor: &'static str,
    pub configured: bool,
    pub live: bool,
    pub fetched_at: Option<i64>,
    pub model_count: Option<usize>,
    pub error: Option<String>,
    /// True when the probe failed and the ids served are the last good
    /// listing (from `fetched_at`), not a fresh one.
    pub serving_cached: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogSnapshot {
    pub entries: Vec<CatalogEntry>,
    pub providers: Vec<ProviderStatus>,
    pub computed_at: i64,
}

static SNAPSHOT: OnceLock<RwLock<Option<CatalogSnapshot>>> = OnceLock::new();

fn cell() -> &'static RwLock<Option<CatalogSnapshot>> {
    SNAPSHOT.get_or_init(|| RwLock::new(None))
}

/// The last computed snapshot, if a refresh has ever completed. `None`
/// before the first tick (or under fleet isolation, where the refresh job
/// never runs) — callers fall back to the static catalog in that case.
pub fn current() -> Option<CatalogSnapshot> {
    cell().read().ok().and_then(|g| g.clone())
}

/// Worker-selectable ids for one amux provider, live-aware. Same contract as
/// `model_catalog::worker_model_ids`: unknown providers return no guesses.
pub fn worker_model_ids(provider: &str) -> Vec<String> {
    let provider = if provider == "claude-code" { "claude" } else { provider };
    let Some(snap) = current() else {
        return model_catalog::worker_model_ids(provider);
    };
    let live: Vec<String> = snap
        .entries
        .iter()
        .filter(|e| e.provider == provider && e.worker_selectable)
        .map(|e| e.id.clone())
        .collect();
    if live.is_empty() {
        // Either this provider isn't one of the three HTTP vendors, or its
        // probe never succeeded — the static list is still merged into every
        // snapshot, so an empty result here means the static filter would
        // also be empty (e.g. a provider id the catalog has never heard of).
        model_catalog::worker_model_ids(provider)
    } else {
        live
    }
}

/// A vendor's key: a Global Vault secret of that name, else server.env (or
/// the process env). The Vault is where the owner keeps keys since its API
/// Keys tab was removed (2026-11-11); server.env stays the fallback so an
/// install that set the key there keeps its live list.
fn vendor_key(home: &Path, env_key: &str) -> Option<String> {
    crate::api::vault_secrets::global_value(home, env_key, "model_catalog")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| crate::api::settings::effective_env(home, env_key).filter(|v| !v.trim().is_empty()))
}

/// Recompute the snapshot: probe every HTTP-listable vendor with a
/// configured key, union each vendor's live ids over the static catalog
/// (keyed by (provider, id) — a probe failure never removes a previously
/// offered id), and swap the merged result in. Read-only against every
/// vendor: one GET each, no mutation.
pub async fn refresh(home: &Path) {
    let static_catalog = model_catalog::catalog();
    let mut by_id: BTreeMap<(&'static str, String), CatalogEntry> = BTreeMap::new();
    for m in &static_catalog {
        by_id.insert((m.provider, m.id.to_string()), CatalogEntry::from(*m));
    }

    let mut last_good = load_last_good(home);
    let now = chrono::Utc::now().timestamp();
    let mut providers = Vec::with_capacity(HTTP_VENDORS.len());
    for (vendor, provider, env_key) in HTTP_VENDORS {
        let prev = last_good.get(vendor).cloned();
        if let Some(p) = prev.as_ref().filter(|p| !p.ids.is_empty() && now - p.fetched_at < FRESH_SECS) {
            merge_ids(&mut by_id, vendor, provider, p.ids.clone(), "live");
            providers.push(ProviderStatus { vendor, configured: true, live: true, fetched_at: Some(p.fetched_at),
                model_count: Some(p.ids.len()), error: None, serving_cached: false });
            continue;
        }
        // Claude: prefer the subscription OAuth token every Claude Code
        // install already holds (super::claude::probe_models_raw) — it
        // answers the public list-models endpoint too, verified live
        // 2026-09-23, so the common case needs no separate credential at
        // all. A standalone ANTHROPIC_API_KEY is the fallback, tried only
        // when the OAuth probe fails. Every other vendor is key-only.
        let attempt: Option<Result<Vec<String>, String>> = if vendor == "anthropic" {
            match super::claude::probe_models_raw().await {
                Ok(ids) => Some(Ok(ids)),
                Err(oauth_err) => {
                    let key = vendor_key(home, env_key);
                    match key {
                        Some(key) => Some(fetch_vendor(vendor, &key).await),
                        // "no_token" means not logged into Claude Code at all
                        // — the same "not configured" state a missing key
                        // means for every other vendor below. Any other
                        // reason (expired, timeout, http_nnn) means a real
                        // credential exists and the probe failed, which
                        // deserves a visible reason rather than reading as
                        // "nothing is configured".
                        None if oauth_err == "no_token" => None,
                        None => Some(Err(oauth_err)),
                    }
                }
            }
        } else {
            let key = vendor_key(home, env_key);
            match key {
                Some(key) => Some(fetch_vendor(vendor, &key).await),
                None => None,
            }
        };
        let configured = attempt.is_some();
        let status = match decide(prev.as_ref(), attempt, now) {
            VendorOutcome::Unconfigured => ProviderStatus {
                vendor, configured, live: false, fetched_at: None, model_count: None, error: None, serving_cached: false,
            },
            VendorOutcome::Live { ids, fetched_at } => {
                let count = ids.len();
                last_good.insert(vendor.to_string(), LastGood { ids: ids.clone(), fetched_at });
                merge_ids(&mut by_id, vendor, provider, ids, "live");
                ProviderStatus {
                    vendor, configured, live: true, fetched_at: Some(fetched_at), model_count: Some(count),
                    error: None, serving_cached: false,
                }
            }
            VendorOutcome::Cached { ids, fetched_at, error } => {
                let count = ids.len();
                tracing::warn!(kind = "provider_model_catalog_serving_cached", vendor, %error,
                    cached_from = fetched_at, n_ids = count, measured = true, n_considered = count,
                    "vendor model listing failed; serving the last good listing instead of dropping to the static catalog");
                merge_ids(&mut by_id, vendor, provider, ids, "cached");
                ProviderStatus {
                    vendor, configured, live: false, fetched_at: Some(fetched_at), model_count: Some(count),
                    error: Some(error), serving_cached: true,
                }
            }
            VendorOutcome::Failed(error) => ProviderStatus {
                vendor, configured, live: false, fetched_at: None, model_count: None, error: Some(error), serving_cached: false,
            },
        };
        providers.push(status);
    }

    save_last_good(home, &last_good);
    let entries: Vec<CatalogEntry> = by_id.into_values().collect();
    let n_live = entries.iter().filter(|e| e.source != "static").count();
    tracing::info!(
        kind = "provider_model_catalog_refreshed",
        measured = true,
        n_considered = entries.len(),
        n_live,
        n_static = entries.len() - n_live,
        vendors_live = providers.iter().filter(|p| p.live).count(),
        vendors_configured = providers.iter().filter(|p| p.configured).count(),
        "live model catalog refreshed"
    );
    let snapshot = CatalogSnapshot {
        entries,
        providers,
        computed_at: chrono::Utc::now().timestamp(),
    };
    if let Ok(mut g) = cell().write() {
        *g = Some(snapshot);
    }
}

/// Union one vendor's ids over the catalog, keyed by (provider, id).
fn merge_ids(
    by_id: &mut BTreeMap<(&'static str, String), CatalogEntry>,
    vendor: &str,
    provider: &'static str,
    ids: Vec<String>,
    source: &'static str,
) {
    for id in ids {
        let existing = by_id.get(&(provider, id.clone()));
        let (model_type, worker_selectable) = classify(existing, &id);
        by_id.insert(
            (provider, id.clone()),
            CatalogEntry { vendor: vendor.to_string(), provider: provider.to_string(), id, model_type, worker_selectable, source },
        );
    }
}

/// Reuse the static classification for an id the static catalog already
/// knows (identical behavior to before this module existed); heuristically
/// classify anything new.
fn classify(existing: Option<&CatalogEntry>, id: &str) -> (String, bool) {
    match existing {
        Some(e) => (e.model_type.clone(), e.worker_selectable),
        None => heuristic_classify(id),
    }
}

/// Best-effort classification for an id no static entry has ever seen.
/// Deliberately conservative: anything whose id LOOKS like a non-agent
/// modality (embeddings, audio, image, moderation, a retired text-completion
/// family) is marked NOT worker_selectable, so a fresh vendor release can
/// never silently appear as a coding-agent choice before anyone has actually
/// checked it — the same bar every hand-typed static entry was held to
/// (model_catalog.rs's own doc comment).
fn heuristic_classify(id: &str) -> (String, bool) {
    let lower = id.to_lowercase();
    const NON_AGENT: &[(&str, &str)] = &[
        ("embed", "embedding"),
        ("whisper", "audio"),
        ("tts", "audio"),
        ("transcribe", "audio"),
        ("dall-e", "image"),
        ("dalle", "image"),
        ("image", "image"),
        ("moderation", "moderation"),
        ("realtime", "realtime"),
        ("audio", "audio"),
        ("clip", "vision-embedding"),
        ("davinci", "legacy"),
        ("babbage", "legacy"),
        ("curie", "legacy"),
    ];
    for (hint, kind) in NON_AGENT {
        if lower.contains(hint) {
            return (kind.to_string(), false);
        }
    }
    if lower.contains("mini") || lower.contains("nano") || lower.contains("flash") || lower.contains("lite") {
        return ("mini".to_string(), true);
    }
    ("flagship".to_string(), true)
}

async fn fetch_vendor(vendor: &str, key: &str) -> Result<Vec<String>, String> {
    match vendor {
        "anthropic" => fetch_anthropic(key).await,
        "openai" => fetch_openai(key).await,
        "google" => fetch_gemini(key).await,
        other => Err(format!("unknown_vendor:{other}")),
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|_| "client".to_string())
}

fn transport_reason(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connect".to_string()
    } else {
        "request".to_string()
    }
}

/// Pull string ids out of `body[list_key][*][id_key]`. Shared by all three
/// vendors — their list-models responses differ only in these two field
/// names.
fn extract_ids(body: &serde_json::Value, list_key: &str, id_key: &str) -> Option<Vec<String>> {
    body.get(list_key)?.as_array().map(|arr| {
        arr.iter()
            .filter_map(|m| m.get(id_key).and_then(|v| v.as_str()).map(str::to_string))
            .collect()
    })
}

/// `GET https://api.anthropic.com/v1/models` — `x-api-key` + the same
/// `anthropic-version` header the usage probe already sends. This is the
/// FALLBACK path only: `refresh` tries `claude::probe_models_raw` (the
/// subscription OAuth token) first, since that needs no separate credential.
/// This function only runs when that fails, using a standalone
/// `ANTHROPIC_API_KEY` — a different, unrelated credential from the OAuth
/// token, kept as a second way in for an account that has one but not the
/// other (e.g. OAuth expired, or a server install with no Claude Code login
/// at all but an API key configured).
async fn fetch_anthropic(key: &str) -> Result<Vec<String>, String> {
    let resp = http_client()?
        .get("https://api.anthropic.com/v1/models")
        .query(&[("limit", "1000")])
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|e| transport_reason(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("http_{}", status.as_u16()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| "bad_shape".to_string())?;
    let ids = extract_ids(&body, "data", "id").ok_or("bad_shape")?;
    if ids.is_empty() {
        return Err("empty".to_string());
    }
    Ok(ids)
}

/// `GET https://api.openai.com/v1/models` — `Authorization: Bearer`. Returns
/// every modality on the account (chat, embeddings, tts, images, ...); the
/// merge/classify step above is what keeps non-agent ids out of
/// `worker_selectable`, same as the hand-typed static catalog already did.
async fn fetch_openai(key: &str) -> Result<Vec<String>, String> {
    let resp = http_client()?
        .get("https://api.openai.com/v1/models")
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .map_err(|e| transport_reason(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("http_{}", status.as_u16()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| "bad_shape".to_string())?;
    let ids = extract_ids(&body, "data", "id").ok_or("bad_shape")?;
    if ids.is_empty() {
        return Err("empty".to_string());
    }
    Ok(ids)
}

/// `GET https://generativelanguage.googleapis.com/v1beta/models` — key as a
/// query param (reqwest encodes it; never logged). Ids come back as
/// `"models/gemini-3.8-flash"`; stripped to the bare id amux uses everywhere
/// else (`--model gemini-3.8-flash`).
async fn fetch_gemini(key: &str) -> Result<Vec<String>, String> {
    let resp = http_client()?
        .get("https://generativelanguage.googleapis.com/v1beta/models")
        .query(&[("key", key), ("pageSize", "1000")])
        .send()
        .await
        .map_err(|e| transport_reason(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("http_{}", status.as_u16()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| "bad_shape".to_string())?;
    let names = extract_ids(&body, "models", "name").ok_or("bad_shape")?;
    let ids: Vec<String> = names
        .into_iter()
        .map(|n| n.strip_prefix("models/").map(str::to_string).unwrap_or(n))
        .collect();
    if ids.is_empty() {
        return Err("empty".to_string());
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_probe_serves_the_last_good_listing_not_the_static_list() {
        let prev = LastGood { ids: vec!["claude-opus-5-5".into()], fetched_at: 100 };
        // Failure with a previous success: the old ids stay, marked cached.
        assert_eq!(
            decide(Some(&prev), Some(Err("timeout".into())), 500),
            VendorOutcome::Cached { ids: vec!["claude-opus-5-5".into()], fetched_at: 100, error: "timeout".into() }
        );
        // Failure with nothing ever listed: an honest failure, not an empty success.
        assert_eq!(decide(None, Some(Err("timeout".into())), 500), VendorOutcome::Failed("timeout".into()));
        let empty = LastGood { ids: vec![], fetched_at: 100 };
        assert_eq!(decide(Some(&empty), Some(Err("x".into())), 500), VendorOutcome::Failed("x".into()));
        // Success replaces the cache.
        assert_eq!(
            decide(Some(&prev), Some(Ok(vec!["a".into()])), 500),
            VendorOutcome::Live { ids: vec!["a".into()], fetched_at: 500 }
        );
        assert_eq!(decide(Some(&prev), None, 500), VendorOutcome::Unconfigured);
    }

    #[test]
    fn the_last_good_listing_survives_a_restart() {
        let home = tempfile::tempdir().unwrap();
        let mut m = BTreeMap::new();
        m.insert("anthropic".to_string(), LastGood { ids: vec!["claude-opus-5-5".into()], fetched_at: 42 });
        save_last_good(home.path(), &m);
        assert_eq!(load_last_good(home.path()), m);
        assert!(load_last_good(tempfile::tempdir().unwrap().path()).is_empty());
    }

    #[test]
    fn known_id_keeps_its_static_classification() {
        let existing = CatalogEntry {
            vendor: "openai".into(),
            provider: "codex".into(),
            id: "gpt-image-2".into(),
            model_type: "image".into(),
            worker_selectable: false,
            source: "static",
        };
        let (model_type, selectable) = classify(Some(&existing), "gpt-image-2");
        assert_eq!(model_type, "image");
        assert!(!selectable);
    }

    #[test]
    fn a_brand_new_id_is_heuristically_classified_never_invented_as_known() {
        let (model_type, selectable) = classify(None, "gpt-7-omega");
        assert_eq!(model_type, "flagship");
        assert!(selectable);
    }

    #[test]
    fn non_agent_modalities_are_never_worker_selectable_even_when_unseen_before() {
        for id in [
            "text-embedding-4-large",
            "whisper-2",
            "gpt-image-3",
            "omni-moderation-latest",
            "tts-2-hd",
        ] {
            let (_, selectable) = classify(None, id);
            assert!(!selectable, "{id} should not be worker_selectable");
        }
    }

    #[test]
    fn mini_family_hints_are_typed_mini_but_still_selectable() {
        let (model_type, selectable) = classify(None, "gpt-7-mini");
        assert_eq!(model_type, "mini");
        assert!(selectable);
    }

    #[test]
    fn extract_ids_reads_the_named_list_and_id_fields() {
        let body = serde_json::json!({
            "data": [{"id": "a"}, {"id": "b"}, {"no_id": true}]
        });
        assert_eq!(
            extract_ids(&body, "data", "id"),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(extract_ids(&body, "missing", "id"), None);
    }

    /// No snapshot has ever been computed in a plain unit test process (the
    /// refresh job only runs from `lib.rs`'s real startup) — every reader
    /// must fall back to the static catalog rather than returning nothing.
    #[test]
    fn with_no_snapshot_worker_model_ids_falls_back_to_the_static_catalog() {
        assert_eq!(
            worker_model_ids("claude"),
            model_catalog::worker_model_ids("claude")
        );
        assert!(!worker_model_ids("claude").is_empty());
    }
}
