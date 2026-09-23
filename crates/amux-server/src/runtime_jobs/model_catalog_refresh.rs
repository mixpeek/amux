//! Live model catalog refresh tick. The fetch/merge/classify logic lives in
//! `provider::live_catalog`; this file is only the schedule.

use crate::provider::live_catalog;

const JOB: &str = super::registry::ids::MODEL_CATALOG_REFRESH;

/// Hourly. Vendor catalogs do not change minute to minute, and a refresh on
/// every tick would be an unforced network call for no fresher an answer
/// (ethos rule 2) — `AMUX_MODEL_CATALOG_REFRESH_SECS` overrides for anyone
/// who wants tighter freshness. `spawn_periodic` ticks once immediately, so
/// a fresh server boot with keys already configured populates the live
/// catalog before the first `/api/models` request in practice, not an hour
/// later.
pub fn spawn() -> super::PeriodicTask {
    super::spawn_periodic(JOB, 3600, || async {
        live_catalog::refresh(&crate::config::amux_home()).await;
    })
}
