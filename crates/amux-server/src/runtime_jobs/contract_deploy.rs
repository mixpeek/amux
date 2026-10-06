//! Contract rule 2b (AH-377): every five minutes, run the post-deploy check of
//! each deploy card whose verified commit has reached production. The logic
//! lives in `api::contract::watch_deploys`; this is only its clock.
use crate::api::AppState;

const JOB: &str = super::registry::ids::CONTRACT_DEPLOY;
const TICK_SECS: u64 = 300;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let state = state.clone();
        async move {
            crate::api::contract::watch_deploys(&state).await;
        }
    })
}
