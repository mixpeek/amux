//! Publish queued social posts when they fall due (api/social_publish.rs).
use crate::api::social::SocialCtx;
use crate::api::social_publish::publish_pass;
use crate::api::AppState;

const JOB: &str = super::registry::ids::SOCIAL_PUBLISH;
const TICK_SECS: u64 = 30;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    let secs = std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(TICK_SECS);
    let ctx = SocialCtx::live();
    super::spawn_periodic(JOB, secs.max(1), move || {
        let (ctx, state) = (ctx.clone(), state.clone());
        async move {
            publish_pass(&ctx, &state).await;
        }
    })
}
