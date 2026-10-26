//! Snapshot each social post's results at 24h, 72h and 7d (api/social.rs).
use crate::api::social::{snapshot_pass, SocialCtx};
use crate::api::AppState;

const JOB: &str = super::registry::ids::SOCIAL_METRICS;
const TICK_SECS: u64 = 600;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    let secs = std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(TICK_SECS);
    let ctx = SocialCtx::live();
    super::spawn_periodic(JOB, secs.max(1), move || {
        let (ctx, state) = (ctx.clone(), state.clone());
        async move {
            snapshot_pass(&ctx, &state).await;
        }
    })
}
