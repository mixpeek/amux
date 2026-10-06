//! The contract's clock (docs/orchestration-contract.md): every five minutes,
//! rule 2b runs the post-deploy check of each deploy card whose verified
//! commit production now contains, and rule 3 starts the fresh reviewer for
//! each card that became eligible for verified. The logic lives in
//! `api::contract`; this is only its clock.
use crate::api::AppState;

const JOB: &str = super::registry::ids::CONTRACT_WATCH;
const TICK_SECS: u64 = 300;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let state = state.clone();
        async move {
            crate::api::contract::watch_deploys(&state).await;
            crate::api::contract::run_reviews(&state).await;
        }
    })
}
