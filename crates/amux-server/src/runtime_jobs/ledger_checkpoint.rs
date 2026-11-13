//! Ledger checkpoints: sign the ledger head every interval and export it
//! (governance PR 10). The work is in `api/ledger_checkpoint.rs`; this is the
//! tick. It wakes every five minutes and signs only when the last checkpoint is
//! older than `AMUX_LEDGER_CHECKPOINT_INTERVAL_S` (default an hour) and the head
//! moved: the server re-execs often and `spawn_periodic` fires on every spawn,
//! so "is one due?" is read from the stored checkpoints, never held in memory.
//! A failed sink is retried on the next tick for the newest checkpoint.

use crate::api::AppState;

const JOB: &str = super::registry::ids::LEDGER_CHECKPOINT;
const TICK_SECS: u64 = 300;

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let st = state.clone();
        async move {
            crate::api::ledger_checkpoint::tick(&st, false).await;
        }
    })
}
