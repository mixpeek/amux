//! Integrations with systems outside the amux process: native Chrome
//! profile management (RR-0092), email/Gmail (RR-0088) and calendar/iCal
//! (RR-0089), plus the degradation registry that makes their health VISIBLE
//! (RR-0073, Invariant 23). API handlers stay under `api/`; this module is
//! the mechanism.
//!
//! The registry exists because of a real diagnosis failure (2026-08-07,
//! recorded in the Python `_gmail_account_health` docstring): token-file
//! presence was conflated with connectedness, two components disagreed about
//! one fact, and three probes were spent in the wrong place. The registry's
//! contract is therefore: states are fed by REAL call outcomes wherever
//! possible, and a state derived from mere configuration says so in its
//! reason ("unverified"), never claiming `Available` on faith.
//!
//! Wiring note (per the RR-0073 seed): this module only EXPORTS
//! [`health_snapshot`]; the integrator adds it to `api/health.rs`. Nothing
//! here touches the health handler.

pub mod browser;
pub mod calendar;
pub mod email;
pub mod gcal_sync;

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// What a caller of an integration can currently expect.
///
/// `Degraded`/`Unavailable` carry a reason because a state without one sends
/// the reader hunting (ethos rule 4: the discriminator belongs where people
/// already look — /health).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationState {
    Available,
    Degraded { reason: String },
    Unavailable { reason: String },
}

impl IntegrationState {
    /// JSON shape for /health: `{"state": "...", "reason": "..."}` with
    /// `reason` omitted for `Available` (nothing to explain).
    pub fn to_json(&self) -> Value {
        match self {
            IntegrationState::Available => json!({ "state": "available" }),
            IntegrationState::Degraded { reason } => {
                json!({ "state": "degraded", "reason": reason })
            }
            IntegrationState::Unavailable { reason } => {
                json!({ "state": "unavailable", "reason": reason })
            }
        }
    }
}

/// Per-integration state registry. Cheap to share (`Arc`), safe to update
/// from any handler. Keys are integration names ("email", "calendar",
/// "calendar_s3", "crm", "crm_sync").
#[derive(Default)]
pub struct IntegrationRegistry {
    inner: Mutex<BTreeMap<String, IntegrationState>>,
}

impl IntegrationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, name: &str, state: IntegrationState) {
        self.inner
            .lock()
            .expect("integration registry poisoned")
            .insert(name.to_string(), state);
    }

    pub fn get(&self, name: &str) -> Option<IntegrationState> {
        self.inner
            .lock()
            .expect("integration registry poisoned")
            .get(name)
            .cloned()
    }

    /// Snapshot for the health endpoint: `{"<name>": {"state": ..}, ..}`.
    /// BTreeMap keeps the order stable so two snapshots diff cleanly.
    pub fn snapshot(&self) -> Value {
        let map = self.inner.lock().expect("integration registry poisoned");
        let mut out = Map::new();
        for (name, state) in map.iter() {
            out.insert(name.clone(), state.to_json());
        }
        Value::Object(out)
    }

    /// Seed initial states from configuration. Called once for the global
    /// registry; tests seed their own instances explicitly.
    ///
    /// Honesty rules (RR-0073):
    /// - email: token files are NOT connectedness (the 2026-08-07 lesson),
    ///   so their presence yields `Degraded{unverified}`, never `Available`.
    ///   Real Gmail call outcomes upgrade/downgrade it (api/email.rs).
    /// - calendar_s3: registered ONLY when AMUX_S3_BUCKET is configured and
    ///   then `Unavailable` until a real publisher exists — the bucket being
    ///   set is a promise this build cannot keep yet.
    /// - crm_sync: same shape for the Lightfield mirror.
    pub fn seed_from_env(&self, amux_home: &Path) {
        let tokens = email::connected_accounts_in(amux_home);
        if tokens.is_empty() {
            self.set(
                "email",
                IntegrationState::Unavailable {
                    reason: "no Gmail accounts connected (no token files in gmail-tokens/)".into(),
                },
            );
        } else {
            self.set(
                "email",
                IntegrationState::Degraded {
                    reason: format!(
                        "{} token file(s) present but unverified — a token file is not \
                         connectedness; state updates on the first real Gmail call",
                        tokens.len()
                    ),
                },
            );
        }
        // Calendar CRUD + iCal generation are pure DB/CPU: available.
        self.set("calendar", IntegrationState::Available);
        self.set("crm", IntegrationState::Available);
        if std::env::var("AMUX_S3_BUCKET").map(|v| !v.trim().is_empty()).unwrap_or(false) {
            self.set(
                "calendar_s3",
                IntegrationState::Unavailable {
                    reason: "AMUX_S3_BUCKET is set but the S3 publisher is not built \
                             (aws-sdk-s3 dep decision pending, RR-0089) — the iCal feed \
                             serves locally but does NOT publish to S3"
                        .into(),
                },
            );
        }
        if std::env::var("LIGHTFIELD_API_KEY").map(|v| !v.trim().is_empty()).unwrap_or(false) {
            self.set(
                "crm_sync",
                IntegrationState::Unavailable {
                    reason: "LIGHTFIELD_API_KEY is set but the external CRM mirror is not \
                             ported (RR-0090) — amux CRM writes are NOT mirrored"
                        .into(),
                },
            );
        }
    }
}

/// Process-global registry, seeded lazily from env + the default amux home.
/// Handlers report real outcomes into this; the health integrator snapshots
/// it.
pub fn global_registry() -> &'static std::sync::Arc<IntegrationRegistry> {
    static REG: OnceLock<std::sync::Arc<IntegrationRegistry>> = OnceLock::new();
    REG.get_or_init(|| {
        let reg = IntegrationRegistry::new();
        reg.seed_from_env(&email::default_amux_home());
        std::sync::Arc::new(reg)
    })
}

/// The one export the health integrator wires up (RR-0073 seed):
/// `health_snapshot()` -> `{"email": {"state": ...}, ...}`.
pub fn health_snapshot() -> Value {
    global_registry().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_set_get_and_transitions() {
        let reg = IntegrationRegistry::new();
        assert_eq!(reg.get("email"), None);
        reg.set("email", IntegrationState::Available);
        assert_eq!(reg.get("email"), Some(IntegrationState::Available));
        // A real failure downgrades; the reason travels with the state.
        reg.set(
            "email",
            IntegrationState::Unavailable { reason: "invalid_grant: token revoked".into() },
        );
        assert_eq!(
            reg.get("email"),
            Some(IntegrationState::Unavailable { reason: "invalid_grant: token revoked".into() })
        );
        // Recovery is a plain set back to Available.
        reg.set("email", IntegrationState::Available);
        assert_eq!(reg.get("email"), Some(IntegrationState::Available));
    }

    #[test]
    fn snapshot_shape_for_health() {
        let reg = IntegrationRegistry::new();
        reg.set("calendar", IntegrationState::Available);
        reg.set("calendar_s3", IntegrationState::Unavailable { reason: "no publisher".into() });
        reg.set("email", IntegrationState::Degraded { reason: "unverified".into() });
        let snap = reg.snapshot();
        assert_eq!(snap["calendar"], json!({ "state": "available" }));
        assert_eq!(
            snap["calendar_s3"],
            json!({ "state": "unavailable", "reason": "no publisher" })
        );
        assert_eq!(snap["email"], json!({ "state": "degraded", "reason": "unverified" }));
        // Available omits reason entirely — nothing to explain.
        assert!(snap["calendar"].get("reason").is_none());
    }

    #[test]
    fn seed_without_tokens_reports_email_unavailable_not_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let reg = IntegrationRegistry::new();
        reg.seed_from_env(dir.path());
        match reg.get("email") {
            Some(IntegrationState::Unavailable { reason }) => {
                assert!(reason.contains("no Gmail accounts connected"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert_eq!(reg.get("calendar"), Some(IntegrationState::Available));
    }

    #[test]
    fn seed_with_token_files_is_degraded_never_available() {
        // Token-file presence is not connectedness (2026-08-07): the seed
        // must NOT claim Available from a directory listing.
        let dir = tempfile::tempdir().unwrap();
        let tok_dir = dir.path().join("gmail-tokens");
        std::fs::create_dir_all(&tok_dir).unwrap();
        std::fs::write(tok_dir.join("someone@example.com.json"), "{}").unwrap();
        let reg = IntegrationRegistry::new();
        reg.seed_from_env(dir.path());
        match reg.get("email") {
            Some(IntegrationState::Degraded { reason }) => {
                assert!(reason.contains("unverified"), "{reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }
}
