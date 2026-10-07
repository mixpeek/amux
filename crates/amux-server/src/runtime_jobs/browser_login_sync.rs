//! Background pass that keeps opted-in browser profiles signed in, by
//! mirroring the owner's Chrome (integrations::browser_login_sync).
//!
//! Every AMUX_BROWSER_LOGIN_SYNC_SECS (default 6 h; 0 disables), and on
//! demand via POST /api/browser/sync-logins. Log signals:
//! `verdict="browser_login_sync"` per profile (copied, removed, signed_in,
//! not_signed_in, skipped) and `"browser_login_sync_pass"` per pass.

use crate::integrations::browser as chrome;
use crate::integrations::browser_login_sync as sync;

const JOB: &str = super::registry::ids::BROWSER_LOGIN_SYNC;
const TICK_SECS: u64 = 6 * 3600;

pub fn tick_secs() -> u64 {
    std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(TICK_SECS)
}

/// One pass over every opted-in profile, or just `only` when given.
pub fn run_pass(only: Option<&str>) -> Vec<sync::ProfileSyncReport> {
    let home = chrome::amux_home();
    let chrome_dir = chrome::chrome_user_data_dir();
    let reg = chrome::registry_load(&home);
    let now = chrono::Utc::now().timestamp();
    let specs = sync::specs_from_registry(&reg);
    let access = crate::api::browser_scope::AccessIndex::build(&crate::api::session_verbs::home(), None);
    let mut out = Vec::new();
    for spec in specs.iter().filter(|s| only.is_none_or(|o| o == s.profile)) {
        let everyone = access.for_profile(&spec.profile)["all_workers"].as_bool().unwrap_or(true);
        let r = if let Some(why) = sync::held_for_scope(&spec.role, everyone) {
            sync::ProfileSyncReport { profile: spec.profile.clone(), skipped: Some(why), ..Default::default() }
        } else {
            let running = chrome::running_snapshot_for(&spec.profile).is_some();
            sync::sync_profile(&home, &chrome_dir, spec, running, now)
        };
        if let Some(why) = &r.skipped {
            tracing::warn!(target: "amux::browser", profile = %r.profile, source = r.source.as_deref().unwrap_or(""),
                why = %why, measured = false, n_considered = spec.sites.len(), verdict = "browser_login_sync",
                "browser login sync skipped a profile");
        } else {
            tracing::info!(target: "amux::browser", profile = %r.profile, source = r.source.as_deref().unwrap_or(""),
                copied = r.copied, removed = r.removed, sessions_persisted = r.sessions_persisted,
                signed_in = r.signed_in.len(), not_signed_in = ?r.not_signed_in, measured = true,
                n_considered = spec.sites.len(), verdict = "browser_login_sync",
                "browser profile mirrored from the owner's Chrome");
        }
        out.push(r);
    }
    tracing::info!(target: "amux::browser", profiles = out.len(),
        synced = out.iter().filter(|r| r.skipped.is_none()).count(), measured = true,
        n_considered = specs.len(), verdict = "browser_login_sync_pass", "browser login sync pass finished");
    out
}

pub fn spawn() -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs().max(60), move || async move {
        if tick_secs() == 0 {
            return;
        }
        let _ = tokio::task::spawn_blocking(|| run_pass(None)).await;
    })
}
