//! Contract rule 5's clock (AH-380): start the next land batch for each
//! repository with nothing running. The logic lives in `api::land_queue`.
use crate::api::AppState;

const JOB: &str = super::registry::ids::LAND_QUEUE;
const TICK_SECS: u64 = 20;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let state = state.clone();
        async move {
            crate::api::land_queue::tick(&state).await;
        }
    })
}
