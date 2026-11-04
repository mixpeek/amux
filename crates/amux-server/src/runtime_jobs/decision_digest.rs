//! Runs [`crate::api::decision_digest::tick_with`]. The classification, the
//! ledger and the render live in the api module; this file is only the loop.
//! Kill switches: `AMUX_DECISION_DIGEST=0` (re-read every tick) and the
//! per-job `AMUX_DECISION_DIGEST_SECS=0`.

use crate::api::AppState;

const JOB: &str = super::registry::ids::DECISION_DIGEST;

fn tick_secs() -> u64 {
    crate::config::env_i64("AMUX_DECISION_DIGEST_TICK_S", 300).max(60) as u64
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let st = state.clone();
        async move {
            let home = crate::config::amux_home();
            let rep = crate::api::decision_digest::tick_with(
                &crate::api::alerts::RealChannels,
                &crate::api::decision_digest::RealActions,
                &st,
                &home,
                chrono::Local::now(),
            )
            .await;
            // Each outcome already logged its own verdict; this is the tick.
            tracing::debug!(ran = rep.ran, digest_sent = rep.digest_sent, applied = rep.applied.len(),
                would_apply = rep.would_apply.len(), verdict = "decision_digest_tick", "decision digest tick");
        }
    })
}
