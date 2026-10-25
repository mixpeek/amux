//! Moves workers started before a Claude sign-in onto the subscription.
//!
//! A worker launch drops ANTHROPIC_API_KEY when the host's Claude is signed in
//! to a subscription, but a worker that was already running keeps the key it
//! started with. When that key is a workspace key over its spend limit, the
//! worker fails every turn with "You have reached your specified workspace API
//! usage limits" while the subscription has usage to spare (sandbox,
//! 2026-10-24). This job relaunches such a worker at its next idle boundary;
//! the relaunch resumes the same conversation. See
//! [`crate::api::session_verbs::relaunch_key_workers_onto_subscription`].

use crate::api::AppState;

const JOB: &str = super::registry::ids::SUBSCRIPTION_RELAUNCH;
const TICK_SECS: u64 = 120;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let state = state.clone();
        async move {
            let (considered, relaunched) =
                crate::api::session_verbs::relaunch_key_workers_onto_subscription(&state).await;
            if considered > 0 {
                tracing::info!(job = JOB, measured = true, n_considered = considered, relaunched,
                    "running Claude workers still on ANTHROPIC_API_KEY after the Claude sign-in");
            }
        }
    })
}
