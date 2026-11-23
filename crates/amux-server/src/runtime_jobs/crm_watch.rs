//! The CRM's own clock (api/crm_agent.rs): fetch each watched public page that
//! is due (websites, careers pages) and emit crm.channel.changed when it shows
//! something new, then attach new mail and LinkedIn messages to the people
//! they are from or to (crm.contact.replied). Every 2 minutes
//! (AMUX_CRM_WATCH_SECS, 0 stops it); off in an isolated (e2e) server.
//! Verdicts: crm_watch_fetch_failed, crm_capture, crm_capture_failed,
//! crm_event_emitted.

use super::registry::ids;
use crate::api::AppState;

const JOB: &str = ids::CRM_WATCH;
const DEFAULT_SECS: i64 = 120;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, 60, move || {
        let state = state.clone();
        async move {
            let every = crate::config::env_i64(&super::per_job_disable_var(JOB), DEFAULT_SECS);
            if every <= 0 || std::env::var("AMUX_ISOLATED").is_ok_and(|v| v == "1") {
                return;
            }
            static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
            let now = chrono::Utc::now().timestamp();
            if now - LAST.load(std::sync::atomic::Ordering::Relaxed) < every {
                return;
            }
            LAST.store(now, std::sync::atomic::Ordering::Relaxed);
            crate::api::crm_agent::tick(&state).await;
        }
    })
}
