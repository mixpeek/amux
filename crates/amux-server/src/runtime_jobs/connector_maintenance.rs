//! Keep renewable credentials usable even when no worker or board job runs.
use crate::api::connectors::ConnectorsCtx;
use std::sync::Arc;
const JOB: &str = super::registry::ids::CONNECTOR_MAINTENANCE;
const TICK_SECS: u64 = 300;

pub fn spawn() -> super::PeriodicTask {
    let secs = std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(TICK_SECS);
    let ctx = Arc::new(ConnectorsCtx {
        home: crate::api::session_verbs::home(),
        http: Arc::new(crate::integrations::email::ReqwestTransport::new()),
    });
    spawn_with(ctx, secs)
}

/// Private process tests supply their own home and actual HTTP provider.
pub fn spawn_with(ctx: Arc<ConnectorsCtx>, secs: u64) -> super::PeriodicTask {
    super::spawn_periodic(JOB, secs.max(1), move || {
        let ctx = ctx.clone();
        async move {
            crate::api::connectors::maintenance_pass(&ctx).await;
        }
    })
}
